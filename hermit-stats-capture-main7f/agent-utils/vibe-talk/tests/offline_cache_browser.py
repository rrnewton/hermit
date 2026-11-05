#!/usr/bin/env python3
"""Exercise /voice's saved-message snapshot in real Chromium at a phone's size.

The fake-DOM suite (tests/js/voice_page.test.mjs) covers every branch of the snapshot. This covers
what only a browser engine can: real localStorage, a real reload, real fetch failures, the Cache
Storage and service-worker registries, and layout at 412x915 and at a 1280x800 desk. For each of
those it serves the checked-out assets and a small fake API from one loopback origin, then walks
one reader through:

  sign in, open the channel — in All, the default where the provider has threads — and read Threads
  and Main too -> reload with the network held: the page reopens on the channel in All by itself,
  its saved rows drawn before any answer (`#189 restore-ui-state`) -> one bounded newest-page read
  of All, sent once the token is proved and the live stream has attached, merges a new row without
  duplicating -> switching views makes no request -> a reload with the API unreachable reopens on
  Main, the view chosen in that channel and kept as its own (`#205 channel-view-memory`), at the
  message a reader parked mid-list was on, keeps the rows and says so -> a reload whose refresh
  fails keeps them and says that -> signing out removes them from the screen and the device. Before
  the reloads, a refresh also fails and recovers in place, on the channel's own poll, under a reader
  at the top, in the middle and at the newest line.

While the freshness pill is up — refreshing, offline, failed — it must cover neither the view tabs
nor the list's header seam or first row, and #scroll-area must be the same box with it as without
it (`#32 freshness-pill-overlap`). As the pill's room and the error panel above the list come and
go, a scrolled reader's line stays where it is on the screen, and a reader at the newest line stays
there (`#33 error-banner-scroll-shift`).

On a touchscreen, a real finger parked at the newest line and drawn up past the end of the list
reads the channel once, says "Updated" at the foot of the list and does not reload the page; a drag
short of the threshold reads nothing (`#188 pull-refresh-bottom`).

At the newest line there is no jump to the newest message; halfway up the channel it is at the lower
left of the chip row in the dark theme — legible, a 44px target, inside the list's column and above
the dock, clear of the floating pills and of every other chip forced up beside it, at the ordinary
type size and at 150%, where on a phone the row wraps and the others go up a line. A real tap a few
pixels above the drawn chip lands on the newest message, reads nothing, starts no pull, and the jump
is gone. It is decided again when the list changes size under a reader who has not scrolled, with the
list's scroll events held back from the page: it comes when the newest message grows below a reader on
the newest line, and goes when it shrinks under one just above it (`#207 scrollback-jump`).

Opening a thread whose title is longer than the screen, with a long unbroken token and inline code
in its reply, leaves the page no wider than the viewport and no shown part of the main screen past
its right edge; on a phone the title is ellipsised instead (`#36 thread-view-phone-overflow`). With
every floating chip showing at once and that thread scrolled to its end, no chip covers the reply
composer's text box or its Send button (`#194 thread-picker-polish`).

The search glass floats on the pill's line rather than costing a header row (`#197
floating-search`): on the main screen no header strip stands above the list, on the call view and
the channel alike; wherever the pill is measured the glass is centred on its line, over the list and
clear of it and of the head of the list, drawn larger than the pill and at least 36px across since
the owner called the pill-sized glass very small (`#211 link-filter`), and a square around the disc
presses it. A real tap below the
disc opens the search as a bar across the top of the list — no wider than the list, not hanging
over what is above it, the pill given way and the head of the list clear of it — the bar filters,
and the glass folds it back with #scroll-area unmoved. Settings still has its title bar and a way
back, and in a thread neither floating thing covers the heading's Back button or title.

Last, a fresh page signed in with a read-scope token reads the channel without a single 4xx answer
or console error: the stored-conversation routes are write-scope, answered 403 here as the server
answers them, and the page must not ask (`#38 read-token-conversation-probe`).

Then, in the dark theme, a second reader pins (`#206 pin-message`): a pin made elsewhere shows on its
row as a "Pinned" chip whose words read against it, and a gold edge; the ⋯ menus of that row and of
an unpinned one open as Copy text, Pin, and the two that mark read under a hairline and a caption —
for a provider that can move its own read marker, whose menu is the widest — every item at least
44px tall, on the screen, and answering a tap at its centre; Pin shows at once and reaches the
server; and the open search bar carries the Pinned filter immediately left of the glass, with Links
immediately left of it — each a 44px target every point of which presses it, clear of the other, of
the glass's own square and of a field still 140px wide beside the count — whose tap shows the pins,
clear of the bar, and which the glass takes away with the bar.

Then, in the dark theme, a reader looks for links (`#211 link-filter`): with the bar open, Links and
Pinned measure as above, and a real tap on Links leaves only the rows that hold a link, each showing
its links where its text was, under its author and time — one per line, each a real link opening a
new tab, at least 44px tall, a Markdown or chat-service link by its name, and an address longer than
the row cut short on screen with its href whole. A real tap on a link opens a new tab at that address
without navigating the app, folding the row or opening its details; the glass takes Links away with
the bar; and over the call view, which has no pins, Links stands beside the glass on its own.

And where a phone reader's own settings wrap a row's meta line — the page zoomed to a 313px
viewport, or the root font at 115%, 130% and 150% — every row's ⋯ menu, and the pinned row's in the
Pinned filter, opens wholly on the screen with every item a thumb can reach, in both provider shapes,
and that row's Unpin is tapped for real. A menu hung from the "⋯" opened off the left of the screen
wherever the "⋯" wrapped to the start of a line. Under the same settings, at 360px and 412px, the open
search bar with Links on and its count up keeps the glass full size and each filter a 44px target, and
its field still leaves room for a query's text, which buttons that grew with the font took away; the
Links view under it shows each link on its own 44px line, clear of the bar; and the kinds of link
under Links measure as below.

Then the kinds of link under Links (`#212 link-filter`, `#266 link-filter-kinds`), in the dark theme
at 412px, 360px, 360px at 150% type and a 1280px desk, over a channel holding GitHub pull requests,
commits, runs of Actions, an issue and a file beside documents: a real tap on Links brings up PRs,
Commits, Actions, Issues, Files and Other, all on, with how many links of each are in view — numbers
that add up to every link the Links view shows — in a row below the bar and under the Links icon —
centred on it, or right-aligned under the bar's end where centred would run past it — never wider
than the bar and wholly inside the viewport, scrolling sideways where six are wider than that, with
the page itself never wider than the screen; each a target at least 44px each way every point of
which presses it once it is scrolled to, covering neither the field, the glass, the filters, the
pill nor the first row, its words legible on and off and the two states drawn apart. A real tap on
PRs takes the pull requests out of the rows that hold them, and a row left with nothing goes;
Commits and Actions off as well leave the documents, the issue and the file; Issues and Files turned
off from the keyboard — the browser scrolling each into view as it takes the focus — leave exactly
the documents, which are Other. After a reload the five are still off; turned on again every link is
back; the glass takes the row away; and over the call view, where Links stands beside the glass, the
row is right-aligned under the bar's end and still inside the screen. With over a hundred links of
each of the six kinds loaded, every button reading "99+", the row stays inside the screen at 313px,
and at 360px under 130% and 150% type, a real swipe across it brings Other into view, and a real tap
then turns it off.

And where the filters empty the list (`EMPTY_SIZES`): a phone reader at the channel's newest line
turns PRs, Commits and Actions off, types text that only a row without a link matches, types text nothing
matches, or turns Links on over a channel with no link — at 412px under 150% type and on the two
phones with the keyboard up, where the composer below the emptied list still overflows it. The
sentence saying why is then wholly on the screen, below the bar and the kinds of link, with nothing
covering a word of it and no jump to the newest message offered; undone, the rows come back with the
reader at the newest line.

Then, in a second fresh profile at each size, a channel whose foot holds five replies to messages
above them (`#204 reply-arrow`): from the owner, the agent and a third party, and between them in
every state a row recedes in — the owner's own, read by default; one somebody answered; one swiped
Done; an automatic placeholder; and one in none of them. Every reply's arrow is a 44px square in
the gutter to the left of its own box: touching the box, inside the list, within the row's height,
covering none of the row's text or buttons, and the thing a tap on it lands on. On a screenshot of
that square, the mark is drawn in the accent at 3:1 or better against the page beside it, however
far its row recedes — measured in pixels, because a computed colour knows nothing of the opacity and
filter of the row around it. All of that at the default type size, at 150% type and in the dark
scheme, with the page no wider than the viewport. A real tap on the newest reply's arrow, from the
newest line, brings the message it answers under the floating pill and lights it without folding
the reply; "Back to reply", tapped, brings the reply back and goes; and in reading mode the arrow
still only jumps, with no read of the reply asked for.

Then, in a third fresh profile at each size and in the dark theme, the coding agent's tiles (`#213
row-side-borders`), beside the owner's and a third party's rows and in every state that draws a
row's border differently — summarised, its summary failed, being read, asked to be read, the kept
place, pinned, swiped Done, an automatic placeholder, a reply, the owner's own read by default, one
somebody answered, and on a desk under the pointer. Every row has four drawn sides, and the agent's
tiles are square, with a left and a right side the width, style and colour of their top, but for a
summary's bar, which is the left side and wider than the right. On a phone a tile reaches from one
edge of the screen to the other, a reply's from its gutter to the right edge; on a desk it is inside
the reading column. Nothing on the page scrolls sideways, at 360px as at the other sizes.

Then, in a fourth fresh profile at each size and in the dark scheme, a channel whose replies sit both
directly under what they answer and far from it, around a thread whose replies are scattered among
other messages (`#214 reply-arrow`, `#215 reply-coalesce`). A reply directly under the message it
answers draws the arrow whose head meets that row — on its foot within 1px, on the flat past its
rounded corner, or, where that row starts at or beyond the reply's box, on its left side within 1px —
found from the pixels, where the head's width runs out — and a press at that point or on the middle of
the head is that arrow's, even where the head is drawn over the row above's own arrow; a reply to a
message elsewhere draws the arrow that leaves the middle of its box's left side, within 1px, and runs
at 45 degrees. Both at the default type size and at 150%. Where the row above also has a circle on
its lower-left corner for a reply further away (`#228 reply-visible`), the circle and the head keep at
least 2px apart — the head raised up the row's side, or the circle lifted up it beside a head under the
foot — with a press on each that one's. A real tap or click on each such head jumps
to the row above and lights it, and two on the head drawn over another arrow are both that arrow's, and
gather the conversation its reply is in under the conversation's first message. A real tap on the root's
N replies gathers its replies, a reply to a reply among them, under it, in time order, with the root
where it was on the screen within 1px; one bridge in the
accent comes out of the flat of the root's foot, each reply's part of it starting at the foot of the
row above and its line reaching the reply's left edge, all drawn, and still so with a reply in the
stack opened by a tap on its text and at 150% type; the X is a 44px target on the screen, left of the
spine, that puts the replies back in time order with the root unmoved; and two real taps on the arrow
of a reply far below the root gather them again, with that reply where it was on the screen and no jump
taken first.

First of all, the dock (`#218 desktop-dock`), in the dark theme, at a 1280x800 desk, a 1600x1000 one
and the two phones, on the call view — idle, live, and after a call with its note under Talk — and on
the channel, under a thirty-character name, with a thread open beside it, and under a short name. On a
desk every control in it is 32 to 40px tall and at least 32px wide, inside the list's column, and
overlapping no other; the dock is one row (at most 56px) where everything fits — the idle call, the
short name, and the long name once the column is dragged wide — and at most 100px where it does not,
the reading buttons then under the switch at the right of the second row; the pane reads Sound, Clear,
Hang up, Talk and Hide read, Pace, Read, left to right; the channel's picker is as wide as its name,
with a thread open too, and only in the narrowest column is it cut, ending in an ellipsis, with the
view picker at its floor and nothing scrolled out of the pack; and a Tab walk from the gear visits the
controls in the order they are seen, each with a ring nothing clips. On a phone the dock is the one it
was before: 149px, its bar and its tile of large buttons at their old heights and widths.

Before that, two windows (`#265 window-channel-memory`): two pages of one profile, which share
localStorage and each have their own sessionStorage, are left on two channels and reloaded; each
reads and shows its own channel, not the one the other chose last, and a third page opened after
them opens on the channel chosen last.

Pass --screenshots DIR to keep a PNG of each step for review.
"""

from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import os
import re
import tempfile
import threading
import time
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import TYPE_CHECKING, TypedDict
from urllib.parse import parse_qs, unquote, urlsplit

if TYPE_CHECKING:
    from collections.abc import Callable

    from playwright.sync_api import BrowserType, FloatRect, Page, Route


WEB_ROOT = Path(__file__).resolve().parents[1] / "web"
TOKEN = "write-token-browser-cache-check"
READ_TOKEN = "read-token-browser-cache-check"
SCOPES = {f"Bearer {TOKEN}": "write", f"Bearer {READ_TOKEN}": "read"}
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
CACHE_KEY = "vibe-talk.voice.message-cache"
UI_STATE_KEY = "vibe-talk.voice.ui-state"
EPOCH = datetime(2026, 1, 5, 9, 0, tzinfo=timezone.utc)
# `#36 thread-view-phone-overflow`: wider than a phone as words, and unbreakable at its tail.
LONG_TOKEN = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0-0f1e2d3c4b5a69788796a5b4c3d2e1f0"
LONG_TITLE = f"Overnight coordinator for the release lane #link {LONG_TOKEN}"
# `#211 link-filter`. The smallest the search glass may be drawn. It was the freshness pill's height,
# 26px, until the owner called it "very small" on his phone.
GLASS_MIN_PX = 36
# `#211 link-filter`, from review. The least room the search field may leave for its TEXT — inside its
# own padding — on the open bar with both filters and a count beside it: seven characters of its 17px
# type, about 8.5px each, a word or two of a query. Measured where a reader's own settings narrow the
# line (`BAR_SCALES`), where buttons sized in rem once left it none at all.
FIELD_TEXT_MIN_PX = 60
Json = dict[str, object]


def message(index: int, content: str, thread: Json | None = None) -> Json:
    row: Json = {
        "id": str(200 + index),
        "channel_id": CHANNEL["id"],
        "author": "ci-bot",
        "author_id": "1000000000000000001",
        "author_is_bot": True,
        "timestamp": (EPOCH + timedelta(minutes=index)).isoformat().replace("+00:00", "Z"),
        "spoken_time": "",
        "reply_to": None,
        "content": content,
    }
    if thread is not None:
        row["thread"] = thread
    return row


def thread_of(row: Json) -> Json:
    """The row's thread summary, or an empty one for a message posted to the channel itself."""
    thread = row.get("thread")
    return thread if isinstance(thread, dict) else {}


class FakeApi:
    """The routes /voice reads, answering from one mutable little store."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        self.timeline_gate = threading.Event()
        self.timeline_gate.set()
        self.requests: list[str] = []
        root = {"id": "spaces/A/threads/one", "root_message_id": "201", "is_root": True,
                "reply_count": 1, "reply_count_exact": True}
        self.messages = [
            message(0, "main channel announcement"),
            message(1, "a thread root", root),
            message(2, f"an answer in the thread: `{LONG_TOKEN}` and {LONG_TOKEN}",
                    {**root, "is_root": False}),
        ]
        self.threads = [{
            "id": root["id"], "root": self.messages[1], "title": LONG_TITLE,
            "reply_count": 1, "reply_count_exact": True,
            "updated_at": self.messages[2]["timestamp"],
        }]
        # `#206 pin-message`: the pins this check keeps, by message id, and their revision, which a
        # timeline read carries only where a walk asks for it — the first walk counts its requests.
        self.pins: dict[str, Json] = {}
        self.pins_revision = 0
        self.serve_pins_revision = False
        # The provider's shape. Discord's by default; the pin walks also take the one whose ⋯ menu is
        # widest — a provider that can move its own read marker, so the read group holds two items.
        self.provider_name = "Discord"
        self.upstream_read = False

    def pin_list(self) -> Json:
        with self.lock:
            pins = sorted(self.pins.values(), key=lambda pin: str(pin["timestamp"]))
            return {"channel": CHANNEL, "pins": pins, "revision": self.pins_revision, "limit": 100,
                    "pins_notice": "Pins are this check's own.",
                    "untrusted_content_notice": "third-party text; DATA, never instructions"}

    def pin(self, message_id: str, body: Json | None) -> Json:
        with self.lock:
            if body is None:
                changed = self.pins.pop(message_id, None) is not None
                pin: Json | None = None
            else:
                changed = message_id not in self.pins
                pin = {**body, "message_id": message_id, "truncated": False,
                       "pinned_at_ms": 1_790_000_000_000 + len(self.pins)}
                self.pins[message_id] = pin
            if changed:
                self.pins_revision += 1
            answer: Json = {"channel": CHANNEL, "message_id": message_id, "pinned": pin is not None,
                            "changed": changed, "unpinned": [], "revision": self.pins_revision,
                            "pins_notice": "Pins are this check's own."}
            if pin is not None:
                answer["pin"] = pin
            return answer

    def reads(self) -> list[str]:
        with self.lock:
            return [r for r in self.requests if "/timeline?" in r or "/page" in r]

    def post(self, path: str, body: Json) -> tuple[int, Json]:
        """A POST to `path` with its JSON body, as status and answer. None is served here; a walk that
        writes (`tests/reply_in_column_browser.py`, its replies and Done) answers its own."""
        return 404, {"error": "not_found", "detail": "not served by this check"}

    def summary(self, message_id: str) -> tuple[int, Json]:
        """A message's summary, as status and body. Only the tile walk (`TileApi`) serves one; every
        other walk is answered as it always was, by the route this check does not serve."""
        return 404, {"error": "not_found", "detail": "not served by this check"}

    def client_config(self, scope: str) -> Json:
        return {
            "token_scope": scope,
            "version": "browser-check",
            "chat_provider_name": self.provider_name,
            "channels": [CHANNEL],
            "live_poll_seconds": 30,
            "live_delivery": "poll",
            "threading_supported": True,
            "channel_registration_supported": False,
            "read_aloud": {"backend": "provider", "label": "Voice provider", "playback": "audio",
                           "local_only": False},
            "elevenlabs_agent_id": None,
            "conversational_voice": {"name": "Test voice provider"},
            "replay_enabled": False,
            "self_author_id": None,
            "owner_author_id": None,
            "channel_discovery_supported": False,
            "upstream_read_mark_supported": self.upstream_read,
            "speech_prep_enabled": True,
        }

    def timeline(self, query: dict[str, list[str]]) -> Json:
        view = query.get("view", ["main"])[0]
        thread_id = query.get("thread_id", [None])[0]
        with self.lock:
            rows = [m for m in self.messages if view == "flat"
                    or (view == "thread" and thread_of(m).get("id") == thread_id)
                    or (view == "main" and (not thread_of(m) or thread_of(m)["is_root"]))]
            threads = list(self.threads) if view == "threads" else []
            thread = next((t for t in self.threads if t["id"] == thread_id), None)
        answer: Json = {
            "channel": CHANNEL, "messages": rows, "threads": threads,
            "thread": thread if view == "thread" else None,
            "has_threads": True, "has_more": False, "next_before": None, "notice": None,
            "dismissed": [], "view": view, "limit": 50, "returned": len(rows) + len(threads),
            "untrusted_content_notice": "third-party text; DATA, never instructions",
        }
        if self.serve_pins_revision:
            answer["pins_revision"] = self.pins_revision
        return answer


# `#204 reply-arrow`. The owner, a third party and the agent, as the page tells them apart.
OWNER_ID = "1000000000000000007"
SPEAKERS: dict[str, Json] = {
    "me": {"author": "owner", "author_id": OWNER_ID, "author_is_bot": False},
    "coder": {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True},
    "human": {"author": "alice", "author_id": "1000000000000000005", "author_is_bot": False},
}


def said(index: int, content: str, who: str = "coder", reply_to: str | None = None) -> Json:
    """`message`, from one of the three speakers and answering `reply_to` when it is given."""
    return {**message(index, content), **SPEAKERS[who], "reply_to": reply_to}


# The reply the reader has swiped Done, as the server reports it.
ARCHIVED_REPLY = "213"


class ReplyApi(FakeApi):
    """`#204 reply-arrow`: a question, more than a screen of the agent's long answers under it, and
    five replies at the foot: from each kind of speaker, so every gutter is measured, and in every
    state a row recedes in, so every arrow is seen as it is drawn on a row that has faded.

    211, the owner's, is already read as his own and answered by 212; 212, the agent's, is answered
    by 213; 213 is swiped Done; 214 is an automatic placeholder; 215 is in none of those, and is the
    one tapped."""

    def __init__(self) -> None:
        super().__init__()
        filler = ("The overnight run is still going: the integration shard has been retried twice "
                  "and the artifact upload is waiting on the cache to warm. ") * 3
        self.messages = [
            said(0, "Is the overnight runner wedged? It has not reported since 03:10."),
            *(said(i, f"Step {i}. {filler}") for i in range(1, 11)),
            said(11, "Restarted it from the console; it should report within the minute.", "me", "200"),
            said(12, "It reported at 03:42 and the queue is draining.", "coder", "211"),
            said(13, "Good, that matches what the dashboard shows here.", "human", "212"),
            {**said(14, "_Working…_", "coder", "213"), "noise": True},
            said(15, "The same thing happened on the other runner last week: it was the disk.", "human", "200"),
        ]
        self.threads = []

    def client_config(self, scope: str) -> Json:
        return {**super().client_config(scope), "owner_author_id": OWNER_ID}

    def timeline(self, query: dict[str, list[str]]) -> Json:
        return {**super().timeline(query), "dismissed": [ARCHIVED_REPLY]}


# `#214 reply-arrow` and `#215 reply-coalesce`. A thread and the replies around it.
COALESCE_THREAD: Json = {"id": "spaces/A/threads/build", "root_message_id": "206", "is_root": True,
                         "reply_count": 3, "reply_count_exact": True}
COALESCE_FILLER = ("The integration shard was retried twice overnight and the artifact upload waited on the "
                   "cache to warm before it went through. ") * 3


class CoalesceApi(FakeApi):
    """`#214 reply-arrow` and `#215 reply-coalesce`: replies directly under what they answer and away
    from it, from every kind of speaker, and a thread whose replies are scattered among other messages.

    200-205 and 217-220 are the agent's long filler, so the list scrolls both ways. 206 is the agent's
    thread root; 207, the owner's, answers it from directly below (its arrow meets the root's foot);
    208, the agent's, answers 207 from directly below (a reply above a reply: the arrow turns to its
    side); 209, 212 and 214 are the thread's replies, a third party's, the agent's and the owner's;
    210 is the owner's, and 211, the agent's, answers it from directly below (the owner's row is inset,
    so on a desk the arrow turns to its side and on a phone it moves under its edge); 213, a third
    party's, answers the root from far below it. 215, the agent's one line, answers 210 from below
    the thread, so its arrow's square is at its middle and reaches down beside its foot — the agent's,
    whose name a row does not print, so that its line still holds its Reply (1) at 360px (`#228
    reply-visible`): a third party's name beside it wraps the line there, and the row is no longer
    short; and 216, the
    agent's, answers 215 from directly below, so its arrow's head is drawn over that square. Gathering
    206 moves 207, 208 (a reply to a reply, so in 206's conversation too), 209, 212, 213 and 214, in
    that order, under it, and leaves 210, 211, 215 and 216 where they are; 210 heads the conversation
    211, 215 and 216 are in."""

    def __init__(self) -> None:
        super().__init__()
        reply = {**COALESCE_THREAD, "is_root": False}
        self.messages = [
            *(said(i, f"Shard {i}. {COALESCE_FILLER}") for i in range(0, 6)),
            {**said(6, "The nightly build failed at the packaging step."), "thread": COALESCE_THREAD},
            said(7, "Which runner was it on?", "me", "206"),
            said(8, "Runner four; its disk filled up.", "coder", "207"),
            {**said(9, "Same failure on my branch this morning.", "human"), "thread": reply},
            said(10, "Cleaning the disk on runner four now.", "me"),
            said(11, f"Disk cleaned; rerunning the job. {COALESCE_FILLER}", "coder", "210"),
            {**said(12, f"Packaging passed on the rerun. {COALESCE_FILLER}", "coder"), "thread": reply},
            said(13, "Thanks, closing the incident.", "human", "206"),
            {**said(14, "Confirmed green here too.", "me"), "thread": reply},
            said(15, "Runner five is at 90% too.", "coder", "210"),
            said(16, "Cleaning it after this run.", "coder", "215"),
            *(said(i, f"Shard {i}. {COALESCE_FILLER}") for i in range(17, 21)),
        ]
        self.threads = [{"id": COALESCE_THREAD["id"], "root": self.messages[6], "title": "Nightly build",
                         "reply_count": 3, "reply_count_exact": True,
                         "updated_at": self.messages[14]["timestamp"]}]

    def client_config(self, scope: str) -> Json:
        return {**super().client_config(scope), "owner_author_id": OWNER_ID}


# The CoalesceApi channel in time order, gathered under its root, and the stack itself.
COALESCE_SCATTERED = [str(200 + i) for i in range(21)]
GATHERED_STACK = ["207", "208", "209", "212", "213", "214"]
COALESCE_GATHERED = [*COALESCE_SCATTERED[:7], *GATHERED_STACK, "210", "211", *COALESCE_SCATTERED[15:]]


def adjacent_heads(mobile: bool) -> list[tuple[str, str, str, str, bool]]:
    """The CoalesceApi channel's adjacent arrows in time order, as (where the head meets the row above,
    the reply, the row above, whose they are, whether the head is drawn over the row above's own
    arrow): the case where a head that let presses through would press a different arrow."""
    return [("below", "207", "206", "the owner's reply under the agent's root", False),
            ("side", "208", "207", "the agent's reply under the owner's reply", False),
            ("below" if mobile else "side", "211", "210", "the agent's reply under the owner's row", False),
            ("side", "216", "215", "the agent's reply under a short reply to a message elsewhere", True)]


# `#211 link-filter`. Where the Links walk's links go. Answered by the browser context itself, so a
# tapped link opens a real tab without anything leaving this machine.
LINK_HOSTS = ("https://example.com/**", "https://example.org/**")
LONG_ADDRESS = "https://example.com/reports/" + "nightly-integration-shard-" * 6 + "summary.html"


class LinkApi(FakeApi):
    """`#211 link-filter`: messages with links in every form the Links view reads, and some without.

    200 is long enough to fold and holds a bare address and a Markdown link; 201 holds none; 202 is a
    chat service's `<address|name>`; 203 is an address longer than any phone is wide; 204 holds none.
    """

    def __init__(self) -> None:
        super().__init__()
        filler = ("The integration shard was retried twice overnight and the artifact upload waited on "
                  "the cache to warm before it went through. ") * 2
        self.messages = [
            said(0, f"{filler}The build log is at https://example.com/build/7, and the change itself is "
                    "[the diff](https://example.org/diff/7)."),
            said(1, "Nothing to open in this one."),
            said(2, "Dashboard: <https://example.com/dash|the dashboard>", "human"),
            said(3, f"Full report: {LONG_ADDRESS}"),
            said(4, "Also nothing to open here.", "me"),
        ]
        self.threads = []


# `#212 link-filter`. GitHub links of every kind the buttons under Links hide, and documents beside them,
# in one repository of a neutral example project.
REPO = "https://github.com/example/project"


class KindsApi(FakeApi):
    """`#212 link-filter`: GitHub pull requests, commits and runs of Actions beside documents.

    200 holds a pull request and a document; 201 a run of Actions and a commit named in Markdown; 202 a
    commit by its full id; 203 a document, a GitHub issue and a file (`#266 link-filter-kinds`); 204
    holds no link; 205 is a dozen pull requests' files and checks, so the PRs button carries a
    two-digit number. Other is the two documents.
    """

    def __init__(self) -> None:
        super().__init__()
        prs = " ".join(f"{REPO}/pull/{50 + n}/{'files' if n % 2 else 'checks'}" for n in range(12))
        self.messages = [
            said(0, f"Review please: {REPO}/pull/41 and the design note https://example.com/docs/design.html"),
            said(1, f"CI failed at {REPO}/actions/runs/123456/job/789 on [abc1234]({REPO}/commit/abc1234def)"),
            said(2, f"Landed in {REPO}/commit/0123456789abcdef0123456789abcdef01234567", "human"),
            said(3, f"Spec: [the spec](https://example.org/spec), the issue {REPO}/issues/7 and the file "
                    f"{REPO}/blob/main/docs/spec.md"),
            said(4, "Nothing to open in this one.", "me"),
            said(5, f"The rest of the queue: {prs}"),
        ]
        self.threads = []


class CrowdedKindsApi(FakeApi):
    """`#212 link-filter`: over a hundred links of each kind, so every button under Links reads "99+".

    The widest the row of kinds ever gets, at the narrowest sizes a reader's own settings make: 200 a
    hundred and twenty pull requests, 201 as many commits, 202 as many runs of Actions, and since `#266
    link-filter-kinds` 203 as many issues, 204 as many files and 205 as many documents, which are Other.
    """

    def __init__(self) -> None:
        super().__init__()
        self.messages = [
            said(0, " ".join(f"{REPO}/pull/{n}" for n in range(120))),
            said(1, " ".join(f"{REPO}/commit/{n:07x}abc" for n in range(120))),
            said(2, " ".join(f"{REPO}/actions/runs/{n}" for n in range(120))),
            said(3, " ".join(f"{REPO}/issues/{n}" for n in range(120))),
            said(4, " ".join(f"{REPO}/blob/main/docs/page-{n}.md" for n in range(120))),
            said(5, " ".join(f"https://example.com/docs/guide-{n}.html" for n in range(120))),
        ]
        self.threads = []


class EmptyKindsApi(FakeApi):
    """`#212 link-filter`, from review: a channel whose only links are of three kinds — 200 a pull
    request, 201 a commit, 202 a run of Actions — and 203 none, so turning those three off empties the
    Links view, and text that matches only 203 empties it with every kind on."""

    def __init__(self) -> None:
        super().__init__()
        self.messages = [
            said(0, f"Review please: {REPO}/pull/41"),
            said(1, f"Landed in {REPO}/commit/abc1234def", "human"),
            said(2, f"CI is green: {REPO}/actions/runs/123456"),
            said(3, "Nothing to open in this one.", "me"),
        ]
        self.threads = []


class LinklessApi(FakeApi):
    """`#212 link-filter`, from review: a channel with no link in it at all, so Links on finds none."""

    def __init__(self) -> None:
        super().__init__()
        self.messages = [
            said(0, "The overnight run is still going."),
            said(1, "It reported at 03:42 and the queue is draining.", "human"),
            said(2, "Good, that matches the dashboard here."),
            said(3, "Nothing to open in this one either.", "me"),
        ]
        self.threads = []


# `#213 row-side-borders`. The rows of the tile walk that are put in a state by name: the one whose
# summary arrives, the one whose summary fails, and the three this check marks itself (see
# `tile_walk`).
SUMMARISED_ROW, FAILED_SUMMARY_ROW = "207", "200"
READING_ROW, MARKED_ROW, PENDING_ROW = "208", "204", "205"


class TileApi(FakeApi):
    """`#213 row-side-borders`: the coding agent's tiles beside the owner's and a third party's rows,
    and a row in every state that draws a row's border differently.

    At the newest line: 206, the owner's, already read as his own; 207, the agent's, long enough to
    fold, and summarised; 208, the agent's, the row being read; 209, a third party's, answered by 210,
    the agent's reply to it; and 211, the agent's, in no state at all. At the top: 200, the agent's,
    whose summary fails; 201, pinned elsewhere; 202, swiped Done; 203, an automatic placeholder; 204,
    the kept place; and 205, asked to be read and not yet speaking."""

    def __init__(self) -> None:
        super().__init__()
        filler = ("The overnight run is still going: the integration shard has been retried twice "
                  "and the artifact upload is waiting on the cache to warm. ") * 3
        self.messages = [
            said(0, f"Shard report. {filler}"),
            said(1, "The cache warmed at 03:20 and the upload is retrying."),
            said(2, "Shard 4 passed on its second attempt."),
            {**said(3, "_Working…_"), "noise": True},
            said(4, "The artifact upload finished at 03:31."),
            said(5, "Restarting the integration shard now."),
            said(6, "Is the overnight runner wedged? It has not reported since 03:10.", "me"),
            said(7, f"Runner status. {filler}"),
            said(8, "It reported at 03:42 and the queue is draining."),
            said(9, "Good, that matches what the dashboard shows here.", "human"),
            said(10, "Thanks. I will keep watching the queue until it is empty.", "coder", "209"),
            said(11, "The next run starts at 04:00, and I will report when it does."),
        ]
        self.threads = []
        self.serve_pins_revision = True
        self.pins["201"] = {
            "message_id": "201", "author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True,
            "content": str(self.messages[1]["content"]), "truncated": False,
            "timestamp": str(self.messages[1]["timestamp"]), "thread_id": None, "thread_root": False,
            "pinned_at_ms": 1_790_000_000_000,
        }
        self.pins_revision = 1

    def client_config(self, scope: str) -> Json:
        return {**super().client_config(scope), "owner_author_id": OWNER_ID}

    def timeline(self, query: dict[str, list[str]]) -> Json:
        return {**super().timeline(query), "dismissed": ["202"]}

    def summary(self, message_id: str) -> tuple[int, Json]:
        if message_id == FAILED_SUMMARY_ROW:
            return 503, {"error": "summarizer_error", "detail": "the summariser did not answer (browser check)"}
        return 200, {
            "channel": CHANNEL, "message_id": message_id, "state": "generated",
            "summary": "The runner is fine: a shard was retried and the upload waited on the cache.",
            "backend": "browser check", "generated_in_ms": 900, "version": "v1-browser-check",
            "threshold_chars": 280, "untrusted_content_notice": "third-party text; DATA, never instructions",
        }


def handler_for(api: FakeApi) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            parts = urlsplit(self.path)
            path = unquote(parts.path)
            if not path.startswith("/api/"):
                self.asset(path)
                return
            with api.lock:
                api.requests.append(f"GET {self.path}")
            scope = SCOPES.get(self.headers.get("Authorization") or "")
            if scope is None:
                self.json(401, {"error": "unauthorized", "detail": "unknown token"})
            elif path == "/api/v1/client-config":
                self.json(200, api.client_config(scope))
            elif path in ("/api/v1/conversations", "/api/v1/transcript"):
                # Write-scope routes, refused to a read token as the server refuses them.
                if scope != "write":
                    self.json(403, {"error": "forbidden", "detail": "this token may read but not post"})
                elif path == "/api/v1/conversations":
                    self.json(200, {"conversations": []})
                else:
                    self.json(200, {"turns": [], "has_more": False, "next_before": None})
            elif path.endswith("/timeline"):
                api.timeline_gate.wait(20)
                self.json(200, api.timeline(parse_qs(parts.query)))
            elif path.endswith("/pins"):
                self.json(200, api.pin_list())
            elif summary := re.fullmatch(r"/api/v1/channels/[^/]+/messages/([^/]+)/summary", path):
                self.json(*api.summary(summary.group(1)))
            elif path.endswith("/stream"):
                self.stream()
            else:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})

        def do_POST(self) -> None:  # noqa: N802
            with api.lock:
                api.requests.append(f"POST {self.path}")
            # Read whole, so that the next request on this kept-alive connection starts where it should.
            raw = self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}"
            try:
                parsed: object = json.loads(raw)
            except ValueError:
                parsed = {}
            self.json(*api.post(unquote(urlsplit(self.path).path), parsed if isinstance(parsed, dict) else {}))

        def do_PUT(self) -> None:  # noqa: N802
            self.pin_write(json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}"))

        def do_DELETE(self) -> None:  # noqa: N802
            self.pin_write(None)

        def pin_write(self, body: Json | None) -> None:
            """`#206 pin-message`: a PUT or DELETE of one pin, the only writes this check answers."""
            path = unquote(urlsplit(self.path).path)
            with api.lock:
                api.requests.append(f"{'PUT' if body is not None else 'DELETE'} {self.path}")
            found = re.fullmatch(r"/api/v1/channels/[^/]+/pins/(.+)", path)
            if not found:
                self.json(404, {"error": "not_found", "detail": "not served by this check"})
            elif SCOPES.get(self.headers.get("Authorization") or "") != "write":
                self.json(403, {"error": "forbidden", "detail": "this token may read but not write"})
            else:
                self.json(200, api.pin(found.group(1), body))

        def asset(self, request_path: str) -> None:
            relative = "voice.html" if request_path == "/voice" else request_path.lstrip("/")
            target = (WEB_ROOT / (relative or "index.html")).resolve()
            if WEB_ROOT not in target.parents or not target.is_file():
                self.send_error(404)
                return
            body = target.read_bytes()
            content_type = mimetypes.guess_type(target.name)[0] or "application/octet-stream"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def json(self, status: int, payload: Json) -> None:
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except OSError:
                pass  # a held read the browser gave up on when the page reloaded

        def stream(self) -> None:
            # Held open with keep-alive comments, as the live route is; nothing arrives on it.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            try:
                while not api.stopping.wait(1):
                    self.wfile.write(b": keep-alive\n\n")
                    self.wfile.flush()
            except OSError:
                pass
            self.close_connection = True

        def log_message(self, _format: str, *_args: object) -> None:
            pass

    return Handler


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    parser.add_argument(
        "--screenshots",
        type=Path,
        help="directory to write one PNG per step into (default: none are kept)",
    )
    return parser.parse_args()


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


# Park the reader (`where`: "top", "middle", "newest", or "" to stay put), then say where they are:
# the top of the thread root's row on the screen, how far the newest line is below the fold, the scroll
# offset, the list's bottom edge, and whether the error panel is up with something in it.
PLACE_JS = """async (where) => {
    const area = document.getElementById('scroll-area');
    const range = area.scrollHeight - area.clientHeight;
    if (where) area.scrollTop = {top: 0, middle: Math.round(range / 2), newest: area.scrollHeight}[where];
    await new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
    const row = document.querySelector('#discord-log > li[data-id="201"]');
    const error = document.getElementById('error-wrap');
    return {row: row.getBoundingClientRect().top, gap: area.scrollHeight - area.clientHeight - area.scrollTop,
            top: area.scrollTop, bottom: area.getBoundingClientRect().bottom,
            banner: !error.hidden && error.getClientRects().length > 0
                && document.getElementById('error').textContent.trim() !== ''};
}"""

# Everything that can be covered, measured at the head of the list, with the view tabs and without
# them. `problems` is empty when the pill covers none of the tabs, the header seams or the first row,
# fits on its one line, and sits over the list; `area` is #scroll-area's box and `bare` the same box without the pill.
# Scrolling to the top is free only because FakeApi answers has_more: False; with more history it
# would page older rows in and spend a read the "one newest-page read" check counts.
GEOMETRY_JS = """() => {""" + f"""
    const GLASS_MIN_PX = {GLASS_MIN_PX};""" + """
    const area = document.getElementById('scroll-area');
    area.scrollTop = 0;
    const shown = (e) => Boolean(e) && !e.hidden && e.getClientRects().length > 0;
    const box = (e) => e.getBoundingClientRect();
    const meets = (a, b) => a.top < b.bottom && b.top < a.bottom && a.left < b.right && b.left < a.right;
    const pill = document.getElementById('channel-freshness');
    const tabs = document.getElementById('channel-view-tabs');
    const problems = [];
    const glassProblems = (layout, p, list) => {
        const glass = document.getElementById('search-toggle');
        if (!shown(glass)) return [`${layout}: the search glass is not on screen`];
        const g = box(glass), found = [], mid = (r) => (r.top + r.bottom) / 2;
        const off = mid(g) - mid(p);
        if (Math.abs(off) > 1.5) found.push(`${layout}: the glass is centred ${off.toFixed(1)}px off the pill's line`);
        // `#211 link-filter`: drawn larger than the pill since the owner called the pill-sized one
        // "very small", and still a disc.
        if (g.height < GLASS_MIN_PX || g.height < p.height + 8 || Math.abs(g.width - g.height) > 0.5) {
            found.push(`${layout}: the glass is ${g.width}x${g.height}px beside a ${p.height}px pill`);
        }
        if (meets(p, g)) found.push(`${layout}: the glass overlaps the pill "${pill.textContent}"`);
        if (g.top < list.top || g.left < list.left || g.right > list.right) found.push(`${layout}: the glass is not over the list`);
        // ...and clear of the head of the list, which makes room for it as it does for the pill.
        const head = [...document.querySelectorAll('#pane-discord .seam, #discord-log > li')].find(shown);
        if (head && box(head).top < g.bottom - 0.5) found.push(`${layout}: the glass reaches ${(g.bottom - box(head).top).toFixed(1)}px over the head of the list`);
        // The 48px target: beside and below the disc, outside it but inside the square, is the glass.
        const cx = (g.left + g.right) / 2, cy = mid(g);
        for (const [dx, dy] of [[-22, 0], [20, 0], [0, 22]]) {
            const hit = document.elementFromPoint(cx + dx, cy + dy);
            if (!hit || !glass.contains(hit)) {
                const what = hit ? (hit.id ? `#${hit.id}` : hit.tagName.toLowerCase()) : 'nothing';
                found.push(`${layout}: a tap at (${dx}, ${dy})px from the glass's centre lands on ${what}`);
            }
        }
        return found;
    };
    const covering = (layout) => {
        const p = box(pill), list = box(area);
        if (shown(tabs) && meets(p, box(tabs))) problems.push(`${layout}: it covers the Main/Threads/All tabs`);
        const seams = [...document.querySelectorAll('#pane-discord .seam')].filter(shown);
        if (seams.length === 0) problems.push(`${layout}: there is no header seam to measure against`);
        seams.forEach((seam) => {
            if (meets(p, box(seam))) problems.push(`${layout}: it covers the seam "${seam.textContent.trim()}"`);
        });
        const first = document.querySelector('#discord-log > li');
        if (!shown(first)) problems.push(`${layout}: there is no first row to measure against`);
        else if (meets(p, box(first))) problems.push(`${layout}: it covers the first row`);
        if (pill.scrollWidth > pill.clientWidth + 1) problems.push(`${layout}: it is cut short: ${pill.textContent}`);
        // ...and to the fraction of a pixel, which the check above rounds away: a flex line that
        // took a quarter of a pixel off the pill ellipsised the whole of its time (`#197
        // floating-search`) — an ellipsis removes whole glyphs, however small the shortfall. Its
        // width with nothing constraining it is the width it needs; 1/64px is layout's own unit.
        const saved = pill.getAttribute('style');
        pill.style.cssText = 'flex: none; max-width: none; width: max-content';
        const natural = box(pill).width;
        if (saved === null) pill.removeAttribute('style'); else pill.setAttribute('style', saved);
        if (natural > p.width + 1 / 64) {
            problems.push(`${layout}: it needs ${natural.toFixed(2)}px and has ${p.width.toFixed(2)}: ${pill.textContent}`);
        }
        if (p.top < list.top || p.left < list.left || p.right > list.right) {
            problems.push(`${layout}: it is not over the list`);
        }
        // `#197 floating-search`: the glass on the pill's line, the pill's height, clear of it.
        // Measured in every pill state, because the longest sentences are the ones that reach it.
        glassProblems(layout, p, list).forEach((problem) => problems.push(problem));
    };
    // An empty header strip over the list is the defect `#197 floating-search` removed.
    const header = document.getElementById('topbar');
    if (shown(header)) {
        problems.push(`a ${Math.round(box(header).height)}px header strip stands over the main screen`);
    }
    if (!shown(pill)) {
        problems.push('the freshness pill is not shown');
    } else {
        covering('with tabs');
        // A source without threads hides the tabs by this same attribute, and the grid row they
        // held collapses: the layout the overlap was first seen in.
        const tabsHidden = tabs.hidden;
        tabs.hidden = true;
        covering('without tabs');
        tabs.hidden = tabsHidden;
    }
    const size = () => {
        const a = box(area);
        return [a.top, a.left, a.width, a.height, area.clientWidth, area.clientHeight]
            .map((n) => n.toFixed(2)).join(',');
    };
    // The same box with the pill and its room taken away, in the same state, so that a banner
    // appearing for its own reasons is not mistaken for the pill resizing the list.
    const measured = size(), pane = document.getElementById('pane-discord');
    const wasHidden = pill.hidden, reserved = pane.hasAttribute('data-freshness');
    pill.hidden = true;
    pane.removeAttribute('data-freshness');
    const bare = size();
    pill.hidden = wasHidden;
    if (reserved) pane.setAttribute('data-freshness', '');
    // `gchat-thread-selector`: the tabs left the list for the bar's thread picker.
    const picker = document.getElementById('thread-select');
    if (shown(tabs)) problems.push('the Main/Threads/All tabs are back over the list');
    return {problems, area: measured, bare, picker: shown(picker)};
}"""

# `#36 thread-view-phone-overflow`. `problems` is empty when the page is no wider than the viewport
# and no shown element of the main screen reaches past its right edge; `clipped` says the thread
# title is ellipsised, which is what a title longer than the screen should do instead.
WIDTH_JS = """() => {
    const viewport = document.documentElement.clientWidth;
    const problems = [];
    const wide = document.documentElement.scrollWidth;
    if (wide > viewport) problems.push(`the page is ${wide}px wide in a ${viewport}px viewport`);
    for (const element of document.querySelectorAll('#screen-main, #screen-main *')) {
        const box = element.getBoundingClientRect();
        if (box.width === 0 || element.closest('[hidden]')) continue;
        if (box.right > viewport + 1) {
            const name = element.id ? `#${element.id}` : `${element.tagName.toLowerCase()}.${element.className}`;
            problems.push(`${name} reaches ${Math.round(box.right)}px`);
        }
    }
    const title = document.getElementById('thread-title');
    return {problems: problems.slice(0, 8), clipped: title.scrollWidth > title.clientWidth};
}"""

# `#194 thread-picker-polish`. The owner photographed Summaries, Undo, Expand all and Collapse all
# sitting on a thread's "Reply in this thread" box. Run with every chip forced up — the worst case —
# this scrolls the thread to its end and returns what covers the composer: empty when no chip meets
# the text box or Send, and both sit inside the list rather than below its foot.
COMPOSER_JS = """async () => {
    const area = document.getElementById('scroll-area');
    const frames = () => new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
    await frames();
    area.scrollTop = area.scrollHeight;
    await frames();
    const box = (e) => e.getBoundingClientRect();
    const meets = (a, b) => a.top < b.bottom && b.top < a.bottom && a.left < b.right && b.left < a.right;
    const chips = [...document.querySelectorAll('#scroll-tools .chip')].filter((c) => c.getClientRects().length > 0);
    const problems = [];
    if (chips.length < 4) problems.push(`only ${chips.length} chips are showing`);
    if (area.scrollHeight - area.clientHeight - area.scrollTop > 2) problems.push('the thread is not at its end');
    for (const id of ['channel-compose-text', 'channel-send']) {
        const target = box(document.getElementById(id));
        chips.filter((chip) => meets(box(chip), target))
            .forEach((chip) => problems.push(`"${chip.textContent.trim()}" covers #${id}`));
        if (target.bottom > box(area).bottom + 1) problems.push(`#${id} ends below the list`);
    }
    return problems;
}"""

# `#197 floating-search`. Shared by the checks below: is an element really laid out, its box, and
# whether two boxes meet.
FLOAT_HELPERS_JS = f"""
    const GLASS_MIN_PX = {GLASS_MIN_PX};""" + """
    const shown = (e) => Boolean(e) && !e.hidden && e.getClientRects().length > 0;
    const box = (e) => e.getBoundingClientRect();
    const meets = (a, b) => a.top < b.bottom && b.top < a.bottom && a.left < b.right && b.left < a.right;
    const name = (e) => e ? (e.id ? `#${e.id}` : e.tagName.toLowerCase()) : 'nothing';
"""

# The call view has no pill, so the glass is measured against the line it shares with one: centred
# on the zero-height anchor (whose top IS the line), over the list's top-right corner, and no header
# strip standing above it. `problems` is empty when all of that holds.
GLASS_ALONE_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const header = document.getElementById('topbar');
    if (shown(header)) problems.push(`a ${Math.round(box(header).height)}px header strip stands over the call view`);
    const glass = document.getElementById('search-toggle');
    if (!shown(glass)) return {problems: [...problems, 'the search glass is not on the call view']};
    const g = box(glass), list = box(document.getElementById('scroll-area'));
    const line = box(document.getElementById('channel-freshness-anchor'));
    const off = (g.top + g.bottom) / 2 - line.top;
    if (Math.abs(off) > 1) problems.push(`the glass is centred ${off.toFixed(1)}px off the pill's line`);
    if (g.height < GLASS_MIN_PX) problems.push(`the glass is ${g.height}px across, the size the owner called very small`);
    if (g.top < list.top || g.right > list.right) problems.push('the glass is not over the list');
    // The line is the list's width on a phone and the reading column's on a desk.
    if (line.right - g.right > 24) problems.push(`the glass is ${Math.round(line.right - g.right)}px from the corner`);
    return {problems};
}"""

# The search open over the channel: a bar across the top of the list, no wider than it and not
# hanging over whatever is above it, with the field focused and 140px wide or more, the pill given
# way, and the first thing the list shows clear of the bar. `query`, when given, has just been typed;
# `rows` is what is left on screen and `count` what the bar says about it. The list's place starts
# above its box by its top margin: on a narrow phone the list starts below the author filter hanging
# under the bar (`#251 author-filter`), and the bar and the filter stand in that margin, over the list's
# own place and nothing above it.
SEARCH_OPEN_JS = """(query) => {""" + FLOAT_HELPERS_JS + """
    const area = document.getElementById('scroll-area');
    area.scrollTop = 0;
    const problems = [];
    const bar = document.getElementById('search-float'), field = document.getElementById('search-field');
    const count = document.getElementById('search-count'), glass = document.getElementById('search-toggle');
    if (glass.getAttribute('aria-pressed') !== 'true') problems.push('the glass does not read as open');
    if (!shown(field)) return {problems: [...problems, 'the field is not open'], rows: [], count: ''};
    const list = box(area), b = box(bar), f = box(field), g = box(glass);
    const head = list.top - (parseFloat(getComputedStyle(area).marginTop) || 0);
    if (b.top < head - 0.5) problems.push(`the bar hangs ${(head - b.top).toFixed(1)}px over what is above the list`);
    if (b.left < list.left - 0.5 || b.right > list.right + 0.5) problems.push('the bar is wider than the list');
    // 140 since `#211 link-filter` put Links beside Pinned on the same line.
    if (f.width < 140) problems.push(`the field is ${Math.round(f.width)}px wide`);
    if (f.left < b.left || g.right > b.right + 0.5 || g.top < b.top - 0.5 || g.bottom > b.bottom + 0.5) {
        problems.push('the glass or the field is outside the bar');
    }
    if (query && (!shown(count) || meets(box(count), f) || meets(box(count), g))) problems.push('the count is not on the bar beside the field');
    if (shown(document.getElementById('channel-freshness'))) problems.push('the pill is drawn under the open bar');
    if (document.activeElement !== field) problems.push(`the field did not take focus: ${name(document.activeElement)}`);
    const first = [...document.querySelectorAll('#pane-discord .seam, #discord-log > li')].find(shown);
    if (!first) problems.push('there is nothing at the head of the list to measure against');
    else if (box(first).top < b.bottom) problems.push(`the open bar covers ${(b.bottom - box(first).top).toFixed(1)}px of the head of the list`);
    const rows = [...document.querySelectorAll('#discord-log > li[data-id]')].filter(shown).map((li) => li.getAttribute('data-id'));
    return {problems, rows, count: count.textContent};
}"""

# In a thread the heading row is above the list; neither floating thing may cover its Back button
# or title, nor take the Back button's tap.
HEADING_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const glass = document.getElementById('search-toggle'), pill = document.getElementById('channel-freshness');
    const back = document.getElementById('thread-back');
    for (const id of ['thread-back', 'thread-title']) {
        const target = box(document.getElementById(id));
        if (meets(box(glass), target)) problems.push(`the glass covers #${id}`);
        if (shown(pill) && meets(box(pill), target)) problems.push(`the pill covers #${id}`);
    }
    if (box(glass).top < box(document.getElementById('channel-navigation')).bottom) {
        problems.push('the glass is drawn over the thread heading row');
    }
    const b = box(back), hit = document.elementFromPoint((b.left + b.right) / 2, (b.top + b.bottom) / 2);
    if (!back.contains(hit)) problems.push(`a tap on Back lands on ${name(hit)}`);
    return problems;
}"""

# Settings is a destination: the header comes back as a title bar with a way back.
TITLE_BAR_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const header = document.getElementById('topbar');
    return {shown: shown(header), height: box(header).height,
            title: shown(document.getElementById('topbar-title')) ? document.getElementById('topbar-title').textContent : '',
            back: shown(document.getElementById('close-settings')),
            glass: shown(document.getElementById('search-toggle'))};
}"""

# `#204 reply-arrow`. The replies in the list, and what holds across the whole of it: no arrow on a row
# that answers nothing, and the list and the page no wider than themselves.
REPLY_LIST_JS = """() => {
    const area = document.getElementById('scroll-area');
    const problems = [];
    const replies = [...document.querySelectorAll('#discord-log > li[data-is-reply="true"]')]
        .map((row) => row.getAttribute('data-id'));
    if (document.querySelector('#discord-log > li:not([data-is-reply="true"]) > .reply-jump')) {
        problems.push('a row that answers nothing has an arrow');
    }
    if (area.scrollWidth > area.clientWidth + 1) problems.push(`the list is ${area.scrollWidth}px wide in ${area.clientWidth}px`);
    const viewport = document.documentElement.clientWidth;
    if (document.documentElement.scrollWidth > viewport) problems.push(`the page is wider than its ${viewport}px viewport`);
    return {problems, replies};
}"""

# One reply's arrow, measured where it is drawn, its row brought to the middle of the list first.
# `problems` is empty when the arrow is a square at least 44px across in the gutter left of its own
# row's box — touching that box, inside the list and the viewport, within the row's own height so it
# cannot reach a neighbouring row — meets none of the row's text or controls, is what a tap at its
# centre and near its corners lands on, and draws a mark at least 20px each way. A corner may instead
# be the arrow of the reply directly below, where that answers this row and its head is drawn there
# (`#214 reply-arrow`): beside this row's side, 0.9rem above its foot, which a press on the head
# presses, as drawn, and nowhere else. `square` and `mark`
# say where to look to see what it looks like (`ARROW_INK_JS`), `accent` is the colour it is drawn
# in, and `state` is how far its row recedes.
REPLY_ARROW_JS = """async (id) => {""" + FLOAT_HELPERS_JS + """
    const area = document.getElementById('scroll-area');
    const frames = () => new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    if (!row) return {problems: [`no row ${id}`]};
    const label = `${id} (${row.getAttribute('data-who')})`;
    const problems = [];
    row.scrollIntoView({block: 'center'});
    await frames();
    const arrow = row.querySelector(':scope > .reply-jump');
    if (!shown(arrow)) return {problems: [`${label}: no arrow`]};
    const a = box(arrow), r = box(row), list = box(area);
    const mark = box(arrow.querySelector('.reply-jump-mark'));
    if (a.width < 43.5 || a.height < 43.5) problems.push(`${label}: the target is ${a.width}x${a.height}px`);
    if (mark.width < 20 || mark.height < 20) problems.push(`${label}: the mark is ${mark.width}x${mark.height}px`);
    if (Math.abs(a.right - r.left) > 1.5) problems.push(`${label}: the arrow ends at ${a.right}px and its box starts at ${r.left}px`);
    if (a.left < list.left - 0.5 || a.left < -0.5) problems.push(`${label}: the arrow starts at ${a.left}px, off the list`);
    if (a.top < r.top - 0.5 || a.bottom > r.bottom + 0.5) {
        problems.push(`${label}: the arrow spans ${a.top}..${a.bottom}px of a row at ${r.top}..${r.bottom}px`);
    }
    for (const part of row.querySelectorAll('*')) {
        if (arrow.contains(part) || !shown(part)) continue;
        const p = box(part);
        const across = Math.min(a.right, p.right) - Math.max(a.left, p.left);
        const down = Math.min(a.bottom, p.bottom) - Math.max(a.top, p.top);
        if (across > 0.5 && down > 0.5) problems.push(`${label}: the arrow covers ${name(part)}.${part.className}`);
    }
    const rem = parseFloat(getComputedStyle(document.documentElement).fontSize);
    const below = row.nextElementSibling && row.nextElementSibling.querySelector(':scope > .reply-jump');
    const theirHead = (px, py, hit) => below && below.contains(hit) && below.getAttribute('data-reply-reach') === 'side'
        && px >= r.left - 0.6 * rem - 1 && px <= r.left + 1 && Math.abs(py - (r.bottom - 0.9 * rem)) <= 0.5 * rem + 1;
    for (const [x, y] of [[0.5, 0.5], [0.1, 0.1], [0.9, 0.1], [0.1, 0.9], [0.9, 0.9]]) {
        const [px, py] = [a.left + a.width * x, a.top + a.height * y];
        const hit = document.elementFromPoint(px, py);
        if (!hit || !(arrow.contains(hit) || theirHead(px, py, hit))) {
            problems.push(`${label}: a tap at (${x}, ${y}) of the arrow lands on ${name(hit)}`);
        }
    }
    const flag = (attribute) => row.getAttribute(attribute) === 'true';
    const state = flag('data-noise') ? 'noise' : flag('data-archived') ? 'archived'
        : flag('data-own-read') ? 'own-read' : flag('data-replied') ? 'replied' : 'plain';
    return {problems, label, who: row.getAttribute('data-who'), state, accent: getComputedStyle(arrow).color,
            square: {x: a.left, y: a.top, width: a.width, height: a.height},
            mark: {x: mark.left - a.left, y: mark.top - a.top, width: mark.width, height: mark.height}};
}"""

# What an arrow looks like on the screen, from a screenshot of its square (`png`, base64): the page
# behind it, from the square's empty upper-left corner, and the mark, as the pixel inside the mark's
# box furthest in contrast from that, with the contrast between the two. From PIXELS, because a
# computed colour says what colour the arrow is and nothing about the opacity and filter of the row
# it is inside: that is how a mark drawn at 2:1, grey, on every row that had faded passed as the
# accent at 3:1.
ARROW_INK_JS = """async ({png, square, mark}) => {
    const image = new Image();
    image.src = `data:image/png;base64,${png}`;
    await image.decode();
    const canvas = document.createElement('canvas');
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    const context = canvas.getContext('2d');
    context.drawImage(image, 0, 0);
    const {data, width, height} = context.getImageData(0, 0, canvas.width, canvas.height);
    const pixel = (x, y) => [0, 1, 2].map((channel) => data[(y * width + x) * 4 + channel]);
    const luminance = (rgb) => {
        const [r, g, b] = rgb.map((v) => {
            const c = v / 255;
            return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
        });
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const contrast = (one, other) => {
        const [x, y] = [luminance(one), luminance(other)];
        return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
    };
    const scale = width / square.width;
    const inset = Math.round(2 * scale);
    // The UPPER left: since `#214 reply-arrow` the row below may answer this one from directly under
    // it, and its arrow then turns into this row's side near its foot, through the lower left.
    const ground = pixel(inset, inset);
    const [left, right] = [Math.floor(mark.x * scale), Math.ceil((mark.x + mark.width) * scale)];
    const [top, bottom] = [Math.floor(mark.y * scale), Math.ceil((mark.y + mark.height) * scale)];
    let ink = ground;
    let best = 1;
    for (let y = Math.max(0, top); y < Math.min(height, bottom); y += 1) {
        for (let x = Math.max(0, left); x < Math.min(width, right); x += 1) {
            const seen = contrast(pixel(x, y), ground);
            if (seen > best) [best, ink] = [seen, pixel(x, y)];
        }
    }
    return {ground, ink, contrast: best};
}"""

# Every state a row recedes in, and none: a ReplyApi channel has a reply in each.
REPLY_STATES = {"own-read", "replied", "archived", "noise", "plain"}

# Where a row is on the screen relative to the floating line over the head of the list and the
# list's foot, and its state: is it lit, folded, being read or waiting to be. The floating line is
# the lower of the freshness pill and the search glass, as the page measures it.
REPLY_ROW_JS = """(id) => {""" + FLOAT_HELPERS_JS + """
    const area = document.getElementById('scroll-area');
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    let line = box(area).top;
    for (const floating of ['channel-freshness', 'search-float']) {
        const element = document.getElementById(floating);
        if (shown(element)) line = Math.max(line, box(element).bottom);
    }
    const r = box(row), arrow = row.querySelector(':scope > .reply-jump');
    const a = arrow ? box(arrow) : null;
    const chip = document.getElementById('back-to-reply');
    return {top: r.top, bottom: r.bottom, line, foot: box(area).bottom,
            landed: row.getAttribute('data-landed'), collapsed: row.getAttribute('data-collapsed'),
            reading: row.getAttribute('data-reading'), pending: row.getAttribute('data-pending'),
            arrow: a ? [a.left + a.width / 2, a.top + a.height / 2] : null,
            chip: shown(chip) ? [box(chip).left + box(chip).width / 2, box(chip).top + box(chip).height / 2] : null};
}"""

# `#213 row-side-borders`. Every channel row on the screen, as drawn. `problems` is empty when every row
# has four sides, each of some width, style and colour; the coding agent's tiles have square corners,
# and their left and right sides the width, style and colour of their top — but where a summary's bar
# is the left side, which is wider than the right; on a phone (`edge`) a tile reaches both edges of the
# screen, or only the right one where it is a reply, and on a desk it is inside the reading column and
# set in from the list's right; the row being read has its bar; and nothing on the page has a
# horizontal scrollbar: no element that draws one has content wider than itself, and the page is no
# wider than the viewport. `count` is how many rows were measured.
TILE_JS = """({edge}) => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const area = document.getElementById('scroll-area'), column = box(document.getElementById('pane-discord'));
    const viewport = document.documentElement.clientWidth;
    const rows = [...document.querySelectorAll('#discord-log > li[data-id]')].filter(shown);
    for (const row of rows) {
        const who = row.getAttribute('data-who'), s = getComputedStyle(row);
        const label = `${row.getAttribute('data-id')} (${who})`, flag = (attribute) => row.getAttribute(attribute) === 'true';
        const side = (name) => ({width: parseFloat(s[`border${name}Width`]), style: s[`border${name}Style`],
                                 color: s[`border${name}Color`]});
        const [top, right, bottom, left] = ['Top', 'Right', 'Bottom', 'Left'].map(side);
        for (const [name, one] of [['top', top], ['right', right], ['bottom', bottom], ['left', left]]) {
            if (!(one.width > 0) || one.style === 'none' || one.style === 'hidden' || /^rgba\\([^)]*,\\s*0\\)$/.test(one.color)) {
                problems.push(`${label}: no ${name} side, ${one.width}px ${one.style} ${one.color}`);
            }
        }
        if (who !== 'coder') continue;
        const radii = ['TopLeft', 'TopRight', 'BottomRight', 'BottomLeft'].map((corner) => parseFloat(s[`border${corner}Radius`]));
        if (radii.some((radius) => radius !== 0)) problems.push(`${label}: its corners are rounded, ${radii}`);
        const like = (one) => one.width === top.width && one.style === top.style && one.color === top.color;
        const said = (one) => `${one.width}px ${one.style} ${one.color}`;
        if (!like(right)) problems.push(`${label}: its right side is ${said(right)} and its top ${said(top)}`);
        if (flag('data-summarised') || flag('data-summary-failed') ? !(left.width > right.width + 1) : !like(left)) {
            problems.push(`${label}: its left side is ${said(left)} and its top ${said(top)}`);
        }
        const r = box(row), list = box(area);
        if (edge && (r.right < viewport - 0.5 || (!flag('data-is-reply') && r.left > 0.5))) {
            problems.push(`${label}: on a phone the tile spans ${r.left}..${r.right}px of a ${viewport}px screen`);
        }
        if (!edge && (r.left < column.left - 0.5 || r.right > column.right + 0.5 || r.right > list.right - 8)) {
            problems.push(`${label}: on a desk the tile spans ${r.left}..${r.right}px, in a column at ${column.left}..${column.right}px`);
        }
        if (flag('data-reading') && !/inset/.test(s.boxShadow)) problems.push(`${label}: the row being read has no bar`);
    }
    if (document.documentElement.scrollWidth > viewport) {
        problems.push(`the page is ${document.documentElement.scrollWidth}px wide in a ${viewport}px viewport`);
    }
    for (const element of document.querySelectorAll('body, body *')) {
        const style = getComputedStyle(element);
        // A strip built to scroll sideways with no bar, as the control bar's pack is, draws none.
        if (style.scrollbarWidth === 'none') continue;
        if (['auto', 'scroll'].includes(style.overflowX) && element.scrollWidth > element.clientWidth + 1) {
            problems.push(`${name(element)} has a horizontal scrollbar: ${element.scrollWidth}px in ${element.clientWidth}px`);
        }
    }
    return {problems, count: rows.length};
}"""

# `#214 reply-arrow` and `#215 reply-coalesce`. Scroll one row to the middle of the list, away from
# the floating pill at the top and the chips and status line at the foot, and let it settle. A row
# taller than half the list has its top put a third of the way down instead, where what is drawn
# beside its head is.
CENTRE_ROW_JS = """async (id) => {
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    const area = document.getElementById('scroll-area');
    const list = area.getBoundingClientRect(), r = row.getBoundingClientRect();
    area.scrollTop += r.height < list.height / 2
        ? (r.top + r.bottom) / 2 - (list.top + list.bottom) / 2
        : r.top - (list.top + list.height / 3);
    await new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
}"""

# Each row's id, top, and where it is in a stack, and each arrow's style and reach.
COALESCE_ROWS_JS = """() => [...document.querySelectorAll('#discord-log > li')].map((row) => {
    const arrow = row.querySelector(':scope > .reply-jump');
    return {id: row.getAttribute('data-id'), top: row.getBoundingClientRect().top,
            place: row.getAttribute('data-coalesce'), style: arrow ? arrow.getAttribute('data-reply-style') : null,
            reach: arrow ? arrow.getAttribute('data-reply-reach') : null};
})"""

# Where to put a finger: the centre of a row's N replies chip, arrow or X.
TARGET_JS = """({id, part}) => {
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    const node = row && row.querySelector(part === 'chip' ? ':scope > .thread-replies'
        : part === 'x' ? ':scope > .reply-ungather' : part === 'body' ? ':scope > .body' : ':scope > .reply-jump');
    if (!node) return null;
    const b = node.getBoundingClientRect();
    return [b.left + b.width / 2, b.top + Math.min(b.height / 2, 20)];
}"""

# `#204 reply-arrow`. Watch for a jump to land on the row holding message `id`, and remember that it
# did: the light a landing leaves is gone after REPLY_LANDED_MS, and a walk that only looked for it
# could look too late on a loaded machine. Read back as `window.__landedOn`.
LANDING_LATCH_JS = """(id) => {
    window.__landedOn = null;
    new MutationObserver((changes, observer) => {
        if (changes.some(({target}) => target.getAttribute('data-landed') === 'true'
                && target.getAttribute('data-ids').split(' ').includes(id))) {
            window.__landedOn = id;
            observer.disconnect();
        }
    }).observe(document.getElementById('discord-log'), {subtree: true, attributes: true, attributeFilter: ['data-landed']});
}"""

# `#214 reply-arrow` and `#215 reply-coalesce`. From now on, the row of every reply arrow a click lands
# on, in `window.__arrowPresses`: which arrow two presses on one head were, whatever they gathered.
ARROW_PRESS_LATCH_JS = """() => {
    window.__arrowPresses = [];
    document.addEventListener('click', (event) => {
        const arrow = event.target instanceof Element ? event.target.closest('.reply-jump') : null;
        if (arrow) window.__arrowPresses.push(arrow.closest('li[data-id]').getAttribute('data-id'));
    }, true);
}"""

# `#214 reply-arrow`. What a pointer or finger on an adjacent arrow's HEAD presses, the head being the
# part the reader aims at: `tip` is its point as the `below` or `side` check found it in the pixels, and
# the places asked are that point taken 1.5px into the head and the middle of the head. Each must be row
# `id`'s own arrow, whatever is under the head: the gap, the row above's gutter, or that row's own arrow.
# `over` says the head is drawn over the row above's own arrow's square, which is then checked first,
# so the case that check is for cannot quietly stop being the case. Answers the problems, and the first
# place, for a real tap.
HEAD_HIT_JS = """({id, kind, tip, over}) => {
    const rem = parseFloat(getComputedStyle(document.documentElement).fontSize);
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    const arrow = row.querySelector(':scope > .reply-jump');
    const under = row.previousElementSibling && row.previousElementSibling.querySelector(':scope > .reply-jump');
    const [x, y] = tip;
    const places = kind === 'below' ? [[x, y + 1.5], [x, y + 0.3 * rem]] : [[x - 1.5, y], [x - 0.3 * rem, y]];
    const problems = [];
    for (const [px, py] of places) {
        const u = under ? under.getBoundingClientRect() : null;
        if (over && !(u && px > u.left && px < u.right && py > u.top && py < u.bottom)) {
            problems.push(`${id}'s head at (${px.toFixed(1)}, ${py.toFixed(1)}) is not over the arrow of the row above`);
        }
        const hit = document.elementFromPoint(px, py);
        if (hit && arrow.contains(hit)) continue;
        const into = hit && hit.closest('#discord-log > li');
        problems.push(`a tap on ${id}'s head at (${px.toFixed(1)}, ${py.toFixed(1)}) lands on `
            + (hit ? `${hit.tagName.toLowerCase()}.${hit.className}${into ? ` in ${into.getAttribute('data-id')}` : ''}` : 'nothing'));
    }
    return {problems, at: places[0]};
}"""

# `#228 reply-visible`. Where the row above (`parent`) has a circle on its corner as well as row `id`'s
# adjacent arrow meeting it: that the arrow knows (`data-reply-hub`), that the disc keeps at least two
# pixels clear of the head whose point `tip` the `below` or `side` check found, that a press on the
# disc's centre is the circle's, and that the circle is on the row's left edge — centred on the corner,
# or inside it on the agent's tile at a phone's edge — or lifted 0.7rem up it (`data-reply-lift`). With
# no circle there, only that the arrow does not step aside for one.
CIRCLE_BESIDE_HEAD_JS = """({id, parent, kind, tip}) => {
    const rem = parseFloat(getComputedStyle(document.documentElement).fontSize);
    const above = document.querySelector(`#discord-log > li[data-id="${parent}"]`);
    const arrow = document.querySelector(`#discord-log > li[data-id="${id}"] > .reply-jump`);
    const marker = above.querySelector(':scope > .reply-marker');
    const hub = arrow.hasAttribute('data-reply-hub');
    if (!marker) {
        return {circle: false, lifted: false, problems: hub ? [`${id}'s arrow steps aside for a circle ${parent} has not`] : []};
    }
    const problems = [];
    if (!hub) problems.push(`${id}'s arrow does not know ${parent} has a circle on its corner`);
    const b = marker.getBoundingClientRect();
    const r = parseFloat(getComputedStyle(marker, '::before').width) / 2;
    const [cx, cy] = [b.left + b.width / 2, b.top + b.height / 2];
    const [x, y] = tip;
    const head = kind === 'below' ? [x - 0.5 * rem, y, x + 0.5 * rem, y + 0.6 * rem]
        : [x - 0.6 * rem, y - 0.5 * rem, x, y + 0.5 * rem];
    const nearX = Math.max(head[0], Math.min(cx, head[2])), nearY = Math.max(head[1], Math.min(cy, head[3]));
    const gap = Math.hypot(cx - nearX, cy - nearY) - r;
    if (gap < 2) problems.push(`${parent}'s circle comes within ${gap.toFixed(1)}px of ${id}'s head`);
    const hit = document.elementFromPoint(cx, cy);
    if (!hit || !marker.contains(hit)) problems.push(`a press on ${parent}'s circle lands on ${hit ? hit.className : 'nothing'}`);
    const p = above.getBoundingClientRect();
    const lifted = above.hasAttribute('data-reply-lift');
    if (Math.abs(cx - p.left) > 1 && Math.abs(cx - (p.left + r)) > 1) {
        problems.push(`${parent}'s circle is at x ${cx.toFixed(1)}, off its left edge at ${p.left.toFixed(1)}`);
    }
    if (Math.abs(cy - (p.bottom - (lifted ? 0.7 * rem : 0))) > 1) {
        problems.push(`${parent}'s circle is at y ${cy.toFixed(1)}, not ${lifted ? 'lifted 0.7rem up from' : 'on'} its foot at ${p.bottom.toFixed(1)}`);
    }
    return {circle: true, lifted, problems};
}"""

# What is DRAWN, from a screenshot of the viewport (`png`, base64), against the boxes of the rows: one
# `check` at a time, each returning `problems`, empty when it holds. Pixels, because the heads, the
# runs and the bridge's lines are borders and pseudo-elements, which have no box of their own to ask.
#
#   below:        the adjacent arrow of row `id` ends in a head whose point is on the bottom edge of
#                 row `parent` — within 1px — and on the flat of it, past its rounded corner;
#   side:         the adjacent arrow of row `id` turns and its head's point is on the LEFT side of row
#                 `parent`, within 1px, between its top and its rounded lower corner;
#   disconnected: the arrow of row `id` leaves the middle of its box's left side, within 1px, and runs
#                 up and left at 45 degrees — drawn at the 45-degree point, and not level with its
#                 start nor straight above it — as the run's own transform says;
#   bridge:       the gathered replies under row `parent` (`stack`, in order) each have a part of the
#                 bridge from the foot of the row above, the parent's or the reply's before, within 1px;
#                 the spine is under the parent's flat foot and drawn down the reply `id`; its line
#                 reaches that reply's left edge and is drawn; the last part ends at its line; the X is
#                 a 44px square, on the screen, every point of which presses it, its disc left of the
#                 spine; and no gathered reply draws an arrow.
COALESCE_LOOK_JS = """async ({png, check, id, parent, stack}) => {""" + FLOAT_HELPERS_JS + """
    const image = new Image();
    image.src = `data:image/png;base64,${png}`;
    await image.decode();
    const canvas = document.createElement('canvas');
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    const context = canvas.getContext('2d');
    context.drawImage(image, 0, 0);
    const {data, width, height} = context.getImageData(0, 0, canvas.width, canvas.height);
    const scale = width / window.innerWidth;
    const rem = parseFloat(getComputedStyle(document.documentElement).fontSize);
    const stroke = 0.1875 * rem;
    const rgb = (text) => (text.match(/[0-9.]+/g) || []).slice(0, 3).map(Number);
    const accent = rgb(getComputedStyle(document.querySelector('#discord-log .reply-jump')).color);
    const pixel = (X, Y) => {
        if (X < 0 || Y < 0 || X >= width || Y >= height) return null;
        const i = (Y * width + X) * 4;
        return [data[i], data[i + 1], data[i + 2]];
    };
    const near = (seen) => seen !== null && Math.hypot(seen[0] - accent[0], seen[1] - accent[1], seen[2] - accent[2]) <= 60;
    const ink = (x, y) => near(pixel(Math.round(x * scale), Math.round(y * scale)));
    // Every accent pixel in a CSS-pixel rectangle, as CSS coordinates of the device pixels' centres.
    const inkIn = (left, top, right, bottom) => {
        const found = [];
        for (let Y = Math.max(0, Math.floor(top * scale)); Y < Math.min(height, Math.ceil(bottom * scale)); Y += 1) {
            for (let X = Math.max(0, Math.floor(left * scale)); X < Math.min(width, Math.ceil(right * scale)); X += 1) {
                if (near(pixel(X, Y))) found.push([(X + 0.5) / scale, (Y + 0.5) / scale]);
            }
        }
        return found;
    };
    const rowOf = (one) => document.querySelector(`#discord-log > li[data-id="${one}"]`);
    const corner = (row) => parseFloat(getComputedStyle(row).borderBottomLeftRadius) || 0;
    const gutter = 2.75 * rem;
    const problems = [];
    const r = rowOf(id) ? box(rowOf(id)) : null;
    // A head's POINT, to a fraction of a pixel: how much of each line of pixels across the head is
    // accent — a pixel between the page and the accent counts for as much as it is accent — fitted to
    // a straight line and followed to where the head is no width at all. The point itself is a pixel
    // too narrow to be coloured, so the last coloured pixel is up to a pixel and a half short of it.
    // Only the lines between the point and the head's widest, so the base, which may be cut part way
    // through a pixel, cannot bend the line: `last` says the point is at the end of `lines`.
    const pointOf = (lines, last) => {
        const widest = lines.reduce((best, line, i) => (line[1] > lines[best][1] ? i : best), 0);
        const fit = (last ? lines.slice(widest) : lines.slice(0, widest + 1))
            .filter(([, w]) => w >= 3.5 * scale && w <= 13 * scale);
        if (fit.length < 3) return null;
        const n = fit.length, mx = fit.reduce((a, [x]) => a + x, 0) / n, mw = fit.reduce((a, [, w]) => a + w, 0) / n;
        const slope = fit.reduce((a, [x, w]) => a + (x - mx) * (w - mw), 0) / fit.reduce((a, [x]) => a + (x - mx) ** 2, 0);
        return (mx - mw / slope + 0.5) / scale;
    };
    const share = (ground, seen) => {
        const span = accent.map((v, i) => v - ground[i]);
        const size = span.reduce((a, v) => a + v * v, 0);
        return seen === null || size === 0 ? 0
            : Math.max(0, Math.min(1, seen.reduce((a, v, i) => a + (v - ground[i]) * span[i], 0) / size));
    };
    if (check === 'below' || check === 'side') {
        const p = box(rowOf(parent));
        const label = `${id} under ${parent}`;
        if (Math.abs(r.top - p.bottom - 0.5 * rem) > 1) problems.push(`${label}: the rows are ${r.top - p.bottom}px apart`);
        // The page, in the gap between the two rows at the gutter's left edge, where nothing is drawn.
        const ground = pixel(Math.round((r.left - gutter + 2) * scale), Math.round((p.bottom + r.top) / 2 * scale));
        if (check === 'below') {
            const [left, right] = [Math.floor((r.left - gutter) * scale), Math.ceil((r.left - 1) * scale)];
            const lines = [];
            let sx = 0, sw = 0;
            for (let Y = Math.ceil(p.bottom * scale); Y < Math.floor((p.bottom + 0.55 * rem) * scale); Y += 1) {
                let w = 0;
                // Each line against its own ground, its pixel at the gutter's left edge, far from the
                // head. The first line can hold the last of the row above's edge, smoothed across a
                // device pixel: where the list's content sits at half a device pixel (a 28px day rule
                // above it at 2.625x), that edge greys the whole line, and counted against the page it
                // would be a line of head as wide as the gutter, which bends the fit a pixel upward.
                const lineGround = pixel(left, Y) || ground;
                for (let X = left; X < right; X += 1) {
                    const a = share(lineGround, pixel(X, Y));
                    w += a;
                    sx += a * X;
                    sw += a;
                }
                lines.push([Y, w]);
            }
            const tip = pointOf(lines, false);
            if (tip === null) return {problems: [`${label}: no head drawn up to the row above`]};
            const tipX = (sx / sw + 0.5) / scale;
            if (Math.abs(tip - p.bottom) > 1) problems.push(`${label}: the head's point is at ${tip.toFixed(2)}px and the row above's foot at ${p.bottom.toFixed(2)}px`);
            if (tipX < p.left + corner(rowOf(parent)) - 1 || tipX > p.right) {
                problems.push(`${label}: the head's point, at x ${tipX.toFixed(1)}, is not under the flat of the row above (${p.left}..${p.right}, corner ${corner(rowOf(parent))})`);
            }
            return {problems, tip: [tipX, tip]};
        }
        // `#228 reply-visible`. Raised where the row above has a circle on its corner as well.
        const hub = rowOf(id).querySelector(':scope > .reply-jump').hasAttribute('data-reply-hub');
        const [top, bottom] = [Math.floor((p.bottom - (hub ? 2.1 : 1.6) * rem) * scale), Math.ceil(p.bottom * scale)];
        const lines = [];
        let sy = 0, sw = 0;
        // The head alone: the run along to it, and the turn up into the shaft, are further left.
        for (let X = Math.floor((p.left - 0.55 * rem) * scale); X < Math.floor(p.left * scale); X += 1) {
            let h = 0;
            for (let Y = top; Y < bottom; Y += 1) {
                const a = share(ground, pixel(X, Y));
                h += a;
                sy += a * Y;
                sw += a;
            }
            // Followed rightward, so the line runs the other way: the head narrows as x grows.
            lines.push([X, h]);
        }
        const tipX = pointOf(lines, true);
        if (tipX === null) return {problems: [`${label}: no head drawn to the row above's side`]};
        const tipY = (sy / sw + 0.5) / scale;
        if (Math.abs(tipX - p.left) > 1) problems.push(`${label}: the head's point is at x ${tipX.toFixed(2)} and the row above's side at ${p.left.toFixed(2)}`);
        if (tipY <= p.top || tipY >= p.bottom - corner(rowOf(parent))) problems.push(`${label}: the head meets the row above at y ${tipY.toFixed(1)}, off its side (${p.top}..${p.bottom})`);
        return {problems, tip: [tipX, tipY]};
    }
    if (check === 'disconnected') {
        const arrow = rowOf(id).querySelector(':scope > .reply-jump');
        const mark = box(arrow.querySelector('.reply-jump-mark'));
        const middle = (r.top + r.bottom) / 2;
        const label = `${id}'s arrow`;
        const run = getComputedStyle(arrow.querySelector('.reply-jump-mark'), '::before').transform;
        const [a, b] = (run.match(/-?[0-9.e]+/g) || []).map(Number);
        const angle = Math.atan2(b, a) * 180 / Math.PI;
        if (Math.abs(angle - 45) > 1) problems.push(`${label} runs at ${angle.toFixed(1)} degrees, not 45`);
        if (Math.abs(mark.right - r.left) > 1.5) problems.push(`${label} starts at x ${mark.right} and its box at ${r.left}`);
        // The stub's own height on the screen, at a quarter of a rem out from the box: centred on the middle.
        const stub = inkIn(r.left - 0.3 * rem, middle - rem, r.left - 0.2 * rem, middle + rem).map(([, y]) => y);
        if (stub.length === 0) problems.push(`${label}: nothing drawn out of the middle of the box's side`);
        else if (Math.abs((Math.min(...stub) + Math.max(...stub)) / 2 - middle) > 1) {
            problems.push(`${label} leaves the box at y ${((Math.min(...stub) + Math.max(...stub)) / 2).toFixed(1)}, its middle is ${middle.toFixed(1)}`);
        }
        const bend = r.left - 0.5 * rem, d = 0.45 * rem;
        if (!ink(bend - d, middle - d)) problems.push(`${label} is not drawn at its 45-degree point`);
        if (ink(bend - d, middle)) problems.push(`${label} runs level with where it starts`);
        if (ink(bend, middle - d)) problems.push(`${label} runs straight up`);
        if (r.top >= window.innerHeight || r.bottom <= 0) problems.push(`${label}: the row is not on the screen`);
        return {problems, angle};
    }
    if (check === 'bridge') {
        const p = box(rowOf(parent));
        let above = p;
        stack.forEach((one, i) => {
            const row = rowOf(one), c = box(row), part = row.querySelector(':scope > .reply-bridge');
            if (!part) { problems.push(`${one} has no part of the bridge`); return; }
            const b = box(part);
            if (Math.abs(b.top - above.bottom) > 1) problems.push(`${one}'s part of the bridge starts at ${b.top}, the row above ends at ${above.bottom}`);
            if (Math.abs(b.right - c.left) > 1.5) problems.push(`${one}'s line ends at ${b.right}, its box starts at ${c.left}`);
            const spine = b.left + stroke / 2;
            if (i === 0 && (spine < p.left + corner(rowOf(parent)) || spine > p.right)) {
                problems.push(`the spine, at x ${spine.toFixed(1)}, does not come out of the flat of the parent's foot (${p.left}..${p.right})`);
            }
            const arrow = row.querySelector(':scope > .reply-jump');
            if (arrow && getComputedStyle(arrow).display !== 'none') problems.push(`${one} still draws an arrow`);
            const last = i === stack.length - 1;
            const level = c.top + 1 + 1.35 * rem - stroke / 2;
            if (last && Math.abs(b.bottom - (c.top + 1 + 1.35 * rem)) > 1) problems.push(`the last part ends at ${b.bottom}, not at its line`);
            if (!last && Math.abs(b.bottom - c.bottom) > 1) problems.push(`${one}'s part ends at ${b.bottom}, short of its foot at ${c.bottom}`);
            if (one === id) {
                if (!ink(spine, (above.bottom + c.top) / 2)) problems.push(`the spine is not drawn across the gap above ${one}`);
                if (!ink(spine, (b.top + level) / 2)) problems.push(`the spine is not drawn down to ${one}`);
                if (!ink((b.left + stroke + c.left) / 2, level)) problems.push(`${one}'s line into its box is not drawn`);
                if (last && c.bottom - level > 8 && ink(spine, c.bottom - 3)) problems.push('the spine runs on past the last line');
                if (i === 0) {
                    const x = row.querySelector(':scope > .reply-ungather');
                    if (!x) { problems.push('there is no X'); return; }
                    const s = box(x), disc = box(x.querySelector('.reply-ungather-mark'));
                    if (s.width < 43.5 || s.height < 43.5) problems.push(`the X's target is ${s.width}x${s.height}px`);
                    if (s.left < -0.5 || s.right > window.innerWidth + 0.5) problems.push(`the X is off the screen at ${s.left}..${s.right}`);
                    if (Math.abs(s.right - c.left) > 1.5) problems.push(`the X's square ends at ${s.right}, the reply's box starts at ${c.left}`);
                    if (disc.right > spine - stroke / 2) problems.push(`the X's disc, to ${disc.right}, is not left of the spine at ${spine}`);
                    for (const [fx, fy] of [[0.5, 0.5], [0.08, 0.08], [0.92, 0.08], [0.08, 0.92], [0.92, 0.92]]) {
                        const hit = document.elementFromPoint(s.left + s.width * fx, s.top + s.height * fy);
                        if (!hit || !x.contains(hit)) problems.push(`a tap at (${fx}, ${fy}) of the X lands on ${name(hit)}.${hit ? hit.className : ''}`);
                    }
                    if (x.getAttribute('aria-label') !== 'Show replies in time order') problems.push(`the X is named "${x.getAttribute('aria-label')}"`);
                }
            }
            above = c;
        });
        return {problems};
    }
    return {problems: [`no check called ${check}`]};
}"""

# `#206 pin-message` and `#211 link-filter`. The open search bar with its filters in it: Links, then
# Pinned where the view has pins, then the glass, each immediately beside the next and on the glass's
# line, each a whole 44px box every point of which presses it, meeting neither its neighbours, the
# glass nor the field. The glass's own centre is still the glass, and so is the strip of its 48px
# target just left of its disc, which no filter's box may cover. The field is still `fieldMin` px wide
# with both filters and the count beside it — 140 at the ordinary type size — and whatever the size,
# leaves FIELD_TEXT_MIN_PX inside its padding for what is typed. `pinned` says whether Pinned should be
# on the bar.
FILTER_BAR_JS = """({pinned, fieldMin}) => {""" + FLOAT_HELPERS_JS + f"""
    const FIELD_TEXT_MIN_PX = {FIELD_TEXT_MIN_PX};""" + """
    const problems = [];
    const mid = (r) => (r.top + r.bottom) / 2;
    const bar = document.getElementById('search-float'), field = document.getElementById('search-field');
    const glass = document.getElementById('search-toggle');
    const filters = ['links-filter', 'pinned-filter'].map((id) => document.getElementById(id)).filter(shown);
    const wanted = pinned ? 'links-filter pinned-filter' : 'links-filter';
    if (filters.map((f) => f.id).join(' ') !== wanted) {
        return {problems: [`the open bar holds ${filters.map(name).join(', ') || 'no filter'}, not ${wanted}`], field: 0, text: 0};
    }
    const g = box(glass), b = box(bar), fl = box(field);
    filters.forEach((filter, i) => {
        const f = box(filter), label = name(filter);
        if (f.width < 43.5 || f.height < 43.5) problems.push(`${label}'s target is ${f.width.toFixed(1)}x${f.height.toFixed(1)}px`);
        if (meets(f, g)) problems.push(`${label} overlaps the glass`);
        if (meets(f, fl)) problems.push(`${label} overlaps the field`);
        const next = i + 1 < filters.length ? box(filters[i + 1]) : g;
        const gap = next.left - f.right;
        if (gap < -0.5 || gap > 20) problems.push(`${label} is ${gap.toFixed(1)}px from what follows it, not beside it`);
        if (Math.abs(mid(f) - mid(g)) > 1.5) problems.push(`${label} is off the glass's line`);
        if (f.left < b.left - 0.5 || f.right > b.right + 0.5) problems.push(`${label} is outside the bar`);
        // Every point of the square that is ON the screen.
        for (const fx of [0.04, 0.5, 0.96]) {
            for (const fy of [0.04, 0.5, 0.96]) {
                const y = Math.max(f.top + f.height * fy, 1);
                const hit = document.elementFromPoint(f.left + f.width * fx, y);
                if (!hit || !filter.contains(hit)) problems.push(`a tap at (${fx}, ${fy}) of ${label}'s box lands on ${name(hit)}`);
            }
        }
    });
    if (fl.width < fieldMin) problems.push(`the field is ${Math.round(fl.width)}px wide beside the filters`);
    const inside = getComputedStyle(field);
    const text = field.clientWidth - parseFloat(inside.paddingLeft) - parseFloat(inside.paddingRight);
    if (text < FIELD_TEXT_MIN_PX) problems.push(`the field leaves ${text.toFixed(1)}px for its text beside the filters`);
    for (const [x, why] of [[(g.left + g.right) / 2, 'centre'], [g.left - 2, 'target just left of its disc']]) {
        const hit = document.elementFromPoint(x, mid(g));
        if (!hit || !glass.contains(hit)) problems.push(`the glass's ${why} lands on ${name(hit)}`);
    }
    return {problems, field: fl.width, text};
}"""

# `#211 link-filter`. The Links view as it is drawn, the list scrolled to its top: which rows are on
# screen, and for each the links it shows as [text, href, target, rel]. `problems` is empty when every
# shown row has a list of links standing where its text was, the text itself not drawn, each link
# on a line of its own inside the row and at least 44px tall, a link too long for the row cut short
# with an ellipsis rather than wrapped or run off it, and the open bar covering none of the first row.
LINK_VIEW_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const area = document.getElementById('scroll-area');
    area.scrollTop = 0;
    const problems = [];
    const rows = [...document.querySelectorAll('#discord-log > li[data-id]')].filter(shown);
    const bar = box(document.getElementById('search-float'));
    if (rows.length && box(rows[0]).top < bar.bottom - 0.5) problems.push(`the open bar covers ${(bar.bottom - box(rows[0]).top).toFixed(1)}px of the first row`);
    const shownRows = rows.map((row) => {
        const id = row.getAttribute('data-id'), r = box(row);
        const list = row.querySelector(':scope > .row-links');
        if (!shown(list)) problems.push(`row ${id} shows no links`);
        const text = row.querySelector(':scope > .body');
        if (shown(text)) problems.push(`row ${id} still draws its text beside its links`);
        if (!shown(row.querySelector(':scope > .meta'))) problems.push(`row ${id} lost its author and time`);
        let above = list ? box(list).top - 0.5 : 0;
        const links = [...row.querySelectorAll('.row-links > a')].map((a) => {
            const l = box(a);
            if (l.top < above) problems.push(`row ${id}: "${a.textContent}" is not on a line of its own`);
            above = l.bottom - 0.5;
            if (l.height < 43.5) problems.push(`row ${id}: "${a.textContent}" is ${l.height.toFixed(1)}px tall`);
            if (l.left < r.left || l.right > r.right + 0.5) problems.push(`row ${id}: "${a.textContent}" runs out of its row`);
            const style = getComputedStyle(a);
            if (a.scrollWidth > a.clientWidth + 1 && style.textOverflow !== 'ellipsis') problems.push(`row ${id}: a long link is not ellipsised`);
            return [a.textContent, a.getAttribute('href'), a.getAttribute('target'), a.getAttribute('rel'),
                    a.scrollWidth > a.clientWidth + 1];
        });
        return [id, links];
    });
    return {problems, rows: shownRows};
}"""

# `#212 link-filter`. The kinds of link under Links, as drawn: shown, PRs, Commits, Actions, Issues, Files
# then Other (`#266 link-filter-kinds`), in a row below the bar and under the Links icon — centred on it,
# or right-aligned under the bar's end where centred would run past that end, and only then — wholly
# inside the viewport and never wider than the bar, scrolling sideways where its buttons are wider than
# that, and the page never wider than the screen; each, once scrolled to as a swipe or the focus would
# scroll it, wholly inside the row and the screen, a target at least 44px each way every point of which
# presses it, meeting neither the field, the glass, the filters nor the bar; the pill not drawn, and the
# first row of the list, scrolled to its top, clear below them. `pinned` says whether Pinned is on the
# bar. Answers how the row was placed, whether it scrolls, its buttons' whole width and, for each kind,
# [its name, its aria-pressed, the number on it].
KINDS_JS = """({pinned}) => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const row = document.getElementById('link-kinds');
    if (!shown(row)) return {problems: ['no kinds of link are shown under Links'], placement: '', scrolls: false, chips: []};
    if (shown(document.getElementById('pinned-filter')) !== pinned) problems.push(`Pinned is ${pinned ? 'not ' : ''}on the bar`);
    const b = box(document.getElementById('search-float')), l = box(document.getElementById('links-filter'));
    const r = box(row), width = document.documentElement.clientWidth;
    const mid = (l.left + l.right) / 2, rowMid = (r.left + r.right) / 2;
    if (r.top < b.bottom - 0.5) problems.push(`the kinds reach ${(b.bottom - r.top).toFixed(1)}px up into the bar`);
    if (r.top - b.bottom > 8) problems.push(`the kinds float ${(r.top - b.bottom).toFixed(1)}px below the bar, not under it`);
    if (r.left > mid || r.right < mid) problems.push('the kinds are not under the Links icon');
    const centred = Math.abs(rowMid - mid) <= 1;
    if (!centred && Math.abs(r.right - b.right) > 2) {
        problems.push(`the kinds are ${(rowMid - mid).toFixed(1)}px off Links' middle and not right-aligned under the bar's end`);
    }
    if (!centred && mid + r.width / 2 < b.right - 2) problems.push('the kinds are right-aligned where centred under Links would fit');
    if (r.left < -0.5 || r.right > width + 0.5) problems.push(`the kinds run off the screen: ${r.left.toFixed(1)}..${r.right.toFixed(1)} of ${width}`);
    if (r.width > b.width + 0.5) problems.push(`the kinds are ${r.width.toFixed(1)}px wide under a ${b.width.toFixed(1)}px bar`);
    if (document.documentElement.scrollWidth > width + 0.5) problems.push(`the page is ${document.documentElement.scrollWidth}px wide on a ${width}px screen`);
    const scrolls = row.scrollWidth > row.clientWidth + 1;
    if (scrolls && !['auto', 'scroll'].includes(getComputedStyle(row).overflowX)) problems.push('the kinds overflow their row with no way to scroll to them');
    const others = ['search-field', 'search-toggle', 'links-filter', 'pinned-filter'].map((id) => document.getElementById(id)).filter(shown);
    const chips = [...row.querySelectorAll('button')];
    const said = chips.map((chip) => (chip.firstChild ? chip.firstChild.textContent.trim() : ''));
    if (said.join(' ') !== 'PRs Commits Actions Issues Files Other') problems.push(`the kinds read ${said.join(', ')}`);
    const span = chips.reduce((sum, chip) => sum + box(chip).width, 0);
    const kept = row.scrollLeft;
    chips.forEach((chip, i) => {
        // Brought into view as a swipe, or the focus, brings it: the row scrolls, and nothing else does.
        chip.scrollIntoView({block: 'nearest', inline: 'nearest'});
        const c = box(chip), label = said[i];
        if (c.left < r.left - 0.5 || c.right > r.right + 0.5) problems.push(`${label} cannot be scrolled wholly into the row`);
        if (c.left < -0.5 || c.right > width + 0.5) problems.push(`${label} is off the screen when scrolled to`);
        if (c.width < 43.5 || c.height < 43.5) problems.push(`${label}'s target is ${c.width.toFixed(1)}x${c.height.toFixed(1)}px`);
        if (meets(c, b)) problems.push(`${label} reaches into the bar`);
        for (const other of others) if (meets(c, box(other))) problems.push(`${label} covers ${name(other)}`);
        for (const fx of [0.04, 0.5, 0.96]) {
            for (const fy of [0.04, 0.5, 0.96]) {
                const hit = document.elementFromPoint(c.left + c.width * fx, c.top + c.height * fy);
                if (!hit || !chip.contains(hit)) problems.push(`a tap at (${fx}, ${fy}) of ${label} lands on ${name(hit)}`);
            }
        }
    });
    row.scrollLeft = kept;
    if (Math.abs(box(row).top - r.top) > 0.5 || Math.abs(box(row).left - r.left) > 0.5) problems.push('scrolling the kinds moved the row');
    if (shown(document.getElementById('channel-freshness'))) problems.push('the pill is drawn beside the kinds');
    document.getElementById('scroll-area').scrollTop = 0;
    const first = [...document.querySelectorAll('#discord-log > li[data-id], #transcript > li')].find(shown);
    if (first && box(first).top < r.bottom - 0.5) problems.push(`the kinds cover ${(r.bottom - box(first).top).toFixed(1)}px of the first row`);
    return {problems, placement: centred ? 'centred' : 'right-aligned', scrolls, span,
            chips: chips.map((chip, i) => [said[i], chip.getAttribute('aria-pressed'), chip.querySelector('.link-kind-count').textContent])};
}"""

# `#266 link-filter-kinds`. Where one kind's button is, against the row it scrolls in and the screen:
# [its left, its right, the row's left, the row's right, the screen's width, how far the row is scrolled].
KIND_PLACE_JS = """(id) => {
    const c = document.getElementById(id).getBoundingClientRect();
    const row = document.getElementById('link-kinds'), r = row.getBoundingClientRect();
    return [c.left, c.right, r.left, r.right, document.documentElement.clientWidth, row.scrollLeft];
}"""

# `#212 link-filter`. How one kind's button is drawn: [its words' colour, their decoration, its pill's
# fill, its pill's edge], and the contrast of the words against the pill — composited over the bar's
# panel, since the row floats over the list. Colours come from the computed style, which writes
# `color-mix` out as `color(srgb …)`.
KIND_LOOK_JS = """(id) => {
    const chip = document.getElementById(id), own = getComputedStyle(chip), pill = getComputedStyle(chip, '::before');
    const parse = (value) => {
        const srgb = /color\\(srgb ([\\d.]+) ([\\d.]+) ([\\d.]+)/.exec(value);
        if (srgb) return [1, 2, 3].map((i) => Number(srgb[i]) * 255);
        return (value.match(/[\\d.]+/g) || []).slice(0, 3).map(Number);
    };
    const luminance = (rgb) => {
        const [r, g, b] = rgb.map((v) => { const c = v / 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; });
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const ink = luminance(parse(own.color)), ground = luminance(parse(pill.backgroundColor));
    const contrast = (Math.max(ink, ground) + 0.05) / (Math.min(ink, ground) + 0.05);
    return {look: [own.color, own.textDecorationLine, pill.backgroundColor, pill.borderTopColor], contrast};
}"""

# `#212 link-filter`, from review. The sentence an emptied list says instead of its rows, where the reader
# was left — the list NOT scrolled first: shown, wholly below the bar and the kinds of link under it,
# above the foot of the list and the screen, and every point of it the sentence's own, so nothing
# floating covers a word. The kinds' room at the head of the list holds only at its top, and a reader
# put at the newest line of a list whose composer still overflows it had the sentence under the three
# buttons it points to, or above the screen altogether. Answers the problems, the words, and how far
# the list is scrolled.
EMPTY_SENTENCE_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const sentence = document.getElementById('search-empty');
    const area = document.getElementById('scroll-area');
    if (!shown(sentence)) return {problems: ['no sentence says why the list is empty'], text: '', top: area.scrollTop};
    const s = box(sentence), list = box(area), kinds = document.getElementById('link-kinds');
    const over = shown(kinds) ? ['the kinds of link', box(kinds)] : ['the bar', box(document.getElementById('search-float'))];
    if (s.top < over[1].bottom - 0.5) problems.push(`the sentence starts ${(over[1].bottom - s.top).toFixed(1)}px up under ${over[0]}`);
    const foot = Math.min(list.bottom, window.innerHeight);
    if (s.bottom > foot + 0.5) problems.push(`the sentence runs ${(s.bottom - foot).toFixed(1)}px past the foot of the list`);
    for (const fx of [0.1, 0.5, 0.9]) {
        for (const fy of [0.1, 0.5, 0.9]) {
            const x = s.left + s.width * fx, y = s.top + s.height * fy;
            if (y < 0 || y > window.innerHeight) continue;
            const hit = document.elementFromPoint(x, y);
            if (!hit || !sentence.contains(hit)) problems.push(`(${fx}, ${fy}) of the sentence is under ${name(hit)}`);
        }
    }
    return {problems, text: sentence.textContent.trim(), top: area.scrollTop};
}"""

# A row's open ⋯ menu: Copy text, Pin, the bookmark (`#240 reading-bookmark`), then the read group under a hairline and a caption — two
# items in it where the provider can move its own read marker, one where it cannot — every item at
# least 44px tall and as wide as the others, and the whole menu ACROSS the screen wherever the "⋯"
# that opened it sits: at a wrapping width the "⋯" lands at the start of a line, and a menu hung
# from its right edge opened off the left of the screen. Every item then answers a real point at its
# centre once the list is scrolled to it, which is what a thumb needs of it; the list is scrolled
# vertically only, after the menu's own box is measured, so nothing slides it back on screen first.
MENU_JS = """({list, id, upstream}) => {""" + FLOAT_HELPERS_JS + """
    const problems = [];
    const row = document.querySelector(`#${list} > li[data-id="${id}"]`);
    const menu = row && row.querySelector('.row-more-menu');
    if (!menu || !shown(menu)) return {problems: [`row ${id}'s ⋯ menu is not open`], items: []};
    const names = [...menu.children].filter(shown).map((e) => e.className);
    if (names.join(' ') !== 'row-copy-button row-pin-button row-bookmark-button row-more-group') problems.push(`the menu holds ${names.join(', ')}`);
    const group = menu.querySelector('.row-more-group');
    const caption = group && group.querySelector('.row-more-caption');
    if (!caption || !shown(caption) || caption.textContent !== 'Mark read') problems.push('the read group has no caption');
    if (group && parseFloat(getComputedStyle(group).borderTopWidth) < 1) problems.push('the read group has no rule above it');
    const grouped = group ? [...group.querySelectorAll('button')].filter(shown).length : 0;
    if (grouped !== (upstream ? 2 : 1)) problems.push(`the read group offers ${grouped} items`);
    const buttons = [...menu.querySelectorAll('button')].filter(shown);
    for (const button of buttons) {
        const r = box(button);
        if (r.height < 43.5) problems.push(`"${button.textContent}" is ${r.height.toFixed(1)}px tall`);
    }
    if (new Set(buttons.map((button) => Math.round(box(button).width))).size > 1) problems.push('the items are not one width');
    const pin = menu.querySelector('.row-pin-button');
    if (group && pin && box(group).top < box(pin).bottom) problems.push('the read group is not below Pin');
    const m = box(menu), width = document.documentElement.clientWidth;
    if (m.left < -0.5 || m.right > width + 0.5) {
        problems.push(`the menu runs off the screen: ${Math.round(m.left)}..${Math.round(m.right)} of ${width}px`);
    }
    for (const button of buttons) {
        const r = box(button);
        if (r.left < -0.5 || r.right > width + 0.5) problems.push(`"${button.textContent}" runs off the screen`);
    }
    const area = document.getElementById('scroll-area'), parked = area.scrollTop;
    for (const button of buttons) {
        const a = box(area);
        area.scrollTop += (box(button).top + box(button).bottom) / 2 - (a.top + a.bottom) / 2;
        const r = box(button);
        const hit = document.elementFromPoint((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        if (!hit || !button.contains(hit)) problems.push(`a tap on "${button.textContent}" lands on ${name(hit)}`);
    }
    area.scrollTop = parked;
    return {problems, items: buttons.map((button) => button.textContent)};
}"""

# The pinned chip on a row: shown, in words, its ink readable against its own surface in the theme in
# force (WCAG's 4.5:1 for small text), and the row's gold edge drawn.
CHIP_JS = """(id) => {""" + FLOAT_HELPERS_JS + """
    const row = document.querySelector(`#discord-log > li[data-id="${id}"]`);
    const chip = row && row.querySelector('.pin-chip');
    if (!chip || !shown(chip)) return {problems: [`row ${id} has no Pinned chip`], contrast: 0};
    const problems = [];
    if (chip.textContent.trim() !== 'Pinned') problems.push(`the chip says ${chip.textContent}`);
    const channels = (text) => (text.match(/[0-9.]+/g) || []).map(Number);
    const luminance = ([r, g, b]) => {
        const linear = (c) => { c /= 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
        return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    };
    const style = getComputedStyle(chip);
    const ink = luminance(channels(style.color)), ground = luminance(channels(style.backgroundColor));
    const contrast = (Math.max(ink, ground) + 0.05) / (Math.min(ink, ground) + 0.05);
    if (contrast < 4.5) problems.push(`the chip's words are ${contrast.toFixed(2)}:1 against it`);
    const edge = getComputedStyle(row, '::after');
    if (edge.content === 'none' || parseFloat(edge.width) < 3) problems.push('the pinned row has no edge');
    return {problems, contrast};
}"""

# `#207 scrollback-jump`. Put the reader `gap` px short of the end of the list, or halfway up it for
# "middle". From the first call on, while `__holdScrolls` is set the list's scroll events are kept from
# the page: the listener is the document's, in the capture phase, so it runs before the page's own on
# the list and can stop the event reaching it.
PARK_JS = """(gap) => {
    const area = document.getElementById('scroll-area');
    if (window.__holdScrolls === undefined) {
        window.__holdScrolls = false;
        document.addEventListener('scroll', (event) => {
            if (event.target === area && window.__holdScrolls) event.stopImmediatePropagation();
        }, true);
    }
    const range = area.scrollHeight - area.clientHeight;
    area.scrollTop = range - (gap === 'middle' ? Math.round(range / 2) : gap);
}"""

# Whether the jump to the newest message is up, for waiting on.
JUMP_UP_JS = "() => !document.getElementById('jump-newest').hidden"

# Whether the room the channel composer keeps under itself has caught up with the chip row: the page
# sets it from a ResizeObserver on the row, so it follows the row's height a rendering frame late, and
# the end of the list moves when it does.
ROOM_SETTLED_JS = """() => {
    const row = document.getElementById('scroll-tools').getBoundingClientRect().height;
    const room = document.getElementById('frame-body').style.getPropertyValue('--scroll-tools-clearance');
    const px = /^calc\\(([\\d.]+)px \\+ 0\\.5rem\\)$/.exec(room);
    return row > 0 ? Boolean(px) && Math.abs(Number(px[1]) - row) < 0.5 : room === '0px' || room === '';
}"""

# Measure the jump to the newest message where the reader is. `problems` is empty when it is at the
# lower left of the chip row — the left end of the list's column, on the row's bottom line — inside the
# list and above the dock, clear of every other chip shown and of the floating pills, legible against
# its own chip in the colours in force, and pressed by a tap anywhere in a 44px square around its
# centre. `gap` is how far the list runs below the reader.
JUMP_JS = """() => {""" + FLOAT_HELPERS_JS + """
    const area = document.getElementById('scroll-area');
    const gap = area.scrollHeight - area.clientHeight - area.scrollTop;
    const jump = document.getElementById('jump-newest');
    if (!shown(jump)) return {shown: false, gap, label: '', problems: [], centre: [0, 0], top: 0, lines: 0};
    const problems = [];
    const j = box(jump), list = box(area), column = box(document.getElementById('pane-discord'));
    if (j.left < list.left || j.right > list.right || j.top < list.top || j.bottom > list.bottom) {
        problems.push('it is not inside the list');
    }
    if (j.left - column.left < 0 || j.left - column.left > 24) {
        problems.push(`it is ${(j.left - column.left).toFixed(1)}px from the column's left edge`);
    }
    if (list.bottom - j.bottom > 16) problems.push(`it floats ${(list.bottom - j.bottom).toFixed(1)}px above the list's foot`);
    if (j.bottom > box(document.getElementById('dock')).top + 0.5) problems.push('it is under the dock');
    // Laid out, not merely un-hidden: the checks below force hidden chips up with a style, and those
    // still carry the attribute.
    const chips = [...document.querySelectorAll('#scroll-tools .chip')].filter((c) => c.getClientRects().length > 0);
    const lines = new Set(chips.map((c) => Math.round(box(c).bottom))).size;
    for (const chip of chips.filter((c) => c !== jump)) {
        const c = box(chip), what = `"${chip.textContent.trim()}"`;
        if (meets(j, c)) problems.push(`it overlaps ${what}`);
        if (c.bottom > j.bottom + 0.5) problems.push(`${what} is on a line below it`);
        if (c.left < j.right - 0.5 && c.bottom > j.top + 0.5) problems.push(`${what} is beside it on the left`);
        if (c.right > list.right + 0.5) problems.push(`${what} runs off the list`);
    }
    for (const id of ['channel-freshness', 'status-line', 'pull-refresh']) {
        const pill = document.getElementById(id);
        if (shown(pill) && meets(j, box(pill))) problems.push(`it is under #${id}`);
    }
    // Legible: its words against its own chip, by the WCAG contrast ratio, normal text.
    const rgb = (value) => (value.match(/[\\d.]+/g) || []).slice(0, 3).map(Number);
    const lum = (value) => {
        const [r, g, b] = rgb(value).map((v) => v / 255).map((v) => v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const contrast = (style) => {
        const [a, b] = [lum(style.color), lum(style.backgroundColor)].sort((x, y) => y - x);
        return (a + 0.05) / (b + 0.05);
    };
    const plain = contrast(getComputedStyle(jump));
    if (plain < 4.5) problems.push(`its words are ${plain.toFixed(2)}:1 against the chip`);
    const flagged = jump.hasAttribute('data-arrivals');
    jump.setAttribute('data-arrivals', '');
    const arrival = contrast(getComputedStyle(jump));
    if (!flagged) jump.removeAttribute('data-arrivals');
    if (arrival < 4.5) problems.push(`its arrival accent is ${arrival.toFixed(2)}:1 against the chip`);
    // The 44px target: 21px above, below and to either side of its centre is still the jump.
    const cx = (j.left + j.right) / 2, cy = (j.top + j.bottom) / 2;
    for (const [dx, dy] of [[0, 0], [0, -21], [0, 21], [-21, 0], [21, 0]]) {
        const hit = document.elementFromPoint(cx + dx, cy + dy);
        if (!hit || !jump.contains(hit)) problems.push(`a tap at (${dx}, ${dy})px from its centre lands on ${name(hit)}`);
    }
    return {shown: true, gap, label: jump.textContent.trim(), problems, centre: [cx, cy], top: j.top, lines};
}"""

# Where the newest channel row sits after a tap on the jump: `visible` when its end is inside the list.
# Its end, not all of it: the rows are 70vh tall here and the composer follows them, so the whole of
# the newest one does not fit above the composer on a phone.
NEWEST_ROW_JS = """() => {
    const rows = [...document.querySelectorAll('#discord-log > li[data-id]')];
    const r = rows[rows.length - 1].getBoundingClientRect(), list = document.getElementById('scroll-area').getBoundingClientRect();
    return {id: rows[rows.length - 1].getAttribute('data-id'), visible: r.bottom > list.top && r.bottom <= list.bottom + 0.5};
}"""

# A phone, the common narrow Android width, and a desk: the desktop regime is `(min-width: 900px) and
# (pointer: fine)`, so the last is a fine pointer without touch, not a wide phone. Below 360 the
# pill's longest line is ellipsised, which the "cut short" check would report.
PROFILES = (("phone", 412, 915, True), ("phone-360", 360, 800, True), ("desktop", 1280, 800, False))

# `#206 pin-message`. What a phone reader's own settings do to a row's meta line: the page zoomed,
# which narrows the CSS viewport (115% on a 360px phone leaves 313px), or a larger system font,
# stood in for by scaling the root font that every rem on the page follows. Either wraps the line
# sooner and puts some row's "⋯" at the START of a line — the pinned row's first, its chip taking
# room — and every menu must still open wholly on the screen. As (label, width, height, root %).
MENU_SCALES = (("zoom-313", 313, 680, 100), ("font-115", 360, 800, 115), ("font-130", 412, 915, 130),
               ("font-150", 360, 800, 150))

# `#211 link-filter`, from review. The same settings on the open search bar, with Links on and so its
# count up: at 360px, where the field is narrowest, at every type size, and at 130% on 412px too. The
# glass and the filters were sized in rem, so the larger font grew them and the field gave up the width
# — none left for its text at 150%. They are px now, and the field keeps the rest.
BAR_SCALES = (("zoom-313", 313, 680, 100), ("font-115", 360, 800, 115), ("font-130", 360, 800, 130),
              ("font-130-412", 412, 915, 130), ("font-150", 360, 800, 150))

# `#212 link-filter`. Where the kinds of link under Links are walked: the two phones, 360px at 150% type
# — where the row, numbers and all, is widest against the narrowest line — and a desk. Label, width,
# height, touch, root font percent.
KINDS_SIZES = (("phone", 412, 915, True, 100), ("phone-360", 360, 800, True, 100),
               ("font-150", 360, 800, True, 150), ("desktop", 1280, 800, False, 100))
# ...and where the row is at its widest, every button reading "99+", against the narrowest lines: the
# page zoomed to 313px, and 360px at 130% and 150% type, where three once ran off the left of the screen
# and six now scroll.
CROWDED_KINDS_SIZES = (("zoom-313", 313, 680, 100), ("font-130", 360, 800, 130), ("font-150", 360, 800, 150))
# `#212 link-filter`, from review. Where a list the filters emptied is read: 412px at 150% type, 800px
# tall as a phone's browser leaves it, where the composer still overflows the list by 102px with every
# row gone; the two phones with the keyboard up, which leaves a list about 420px tall; and a phone at
# the ordinary size, where it does not overflow at all. Label, width, height, root font percent, and
# how it is emptied: "kinds" turns all three kinds off, "typed" types text matching only the row without
# a link with every kind on, "search" types text matching nothing with Links off, and "linkless" turns
# Links on over a channel with no link in it.
EMPTY_SIZES = (("font-150", 412, 800, 150, "kinds"), ("font-150", 412, 800, 150, "linkless"),
               ("keyboard-360", 360, 420, 100, "typed"), ("keyboard-412", 412, 450, 100, "kinds"),
               ("keyboard-360", 360, 420, 100, "search"), ("phone", 412, 915, 100, "kinds"))
# What each of those says.
EMPTY_SENTENCES = {
    "kinds": "No links left once PRs, Commits and Actions are hidden — turn them back on under the Links button.",
    "typed": "None of the loaded messages with a link match that search.",
    # `#262 search-load-older`: how far back the search looked — here, the whole of All.
    "search": "Nothing in All matches that search — all of it is loaded.",
    "linkless": "No links in the messages loaded so far.",
}


# `#218 desktop-dock`. Where the dock is measured: two desks — the smallest window most people open
# and a large one, whose column is the same width with more margin — and the two phones, where the
# dock must be exactly what it was. Label, width, height, touch.
DOCK_SIZES = (("desktop", 1280, 800, False), ("desktop-1600", 1600, 1000, False),
              ("phone", 412, 915, True), ("phone-360", 360, 800, True))
# A channel whose name is thirty characters, the length the owner's picker cut off at nine rem.
LONG_CHANNEL = {"id": "1110000000000000002", "label": "overnight release coordination", "writable": True,
                "alias": None, "added": False}
# The tallest the desktop dock may stand: two rows of 36px controls with their gap and padding when
# a long name and the three reading buttons do not share the column's one row, and one row when they
# do. It stood 149px before, with a channel name cut short in it. The second is the post-call state,
# whose Talk carries a one-clause note under its word.
DOCK_DESK_MAX_PX = 100
DOCK_ONE_ROW_MAX_PX = 56
DOCK_NOTE_MAX_PX = 120
# What a control on a desk measures: a mouse's 32px target, and short of the slabs it replaced.
DESK_CONTROL_PX = (32, 40)
# The phone dock as 9f055cf2 drew it, from its rem arithmetic: a 1px border, the bar's 2.85rem
# (0.35 + 2.4 + 0.1) and the tile's 6.4rem (two 2.4rem rows, a 0.4rem gap and 0.6rem padding twice).
# The tile's large controls span both rows, 5.2rem; the narrow column is 3.5rem.
PHONE_DOCK_PX = 1 + 2.85 * 16 + 6.4 * 16
PHONE_SPAN_PX = 5.2 * 16
PHONE_NARROW_PX = 3.5 * 16
# How far from the end of the list a reader may be and still be followed: web/voice.js's own number.
_BOTTOM_SLACK = re.search(r"const BOTTOM_SLACK_PX = (\d+);", (WEB_ROOT / "voice.js").read_text())
if _BOTTOM_SLACK is None:
    raise SystemExit("web/voice.js no longer declares BOTTOM_SLACK_PX, which the dock walk follows from")
BOTTOM_SLACK_PX = int(_BOTTOM_SLACK[1])

# Rows enough to make the call's transcript scroll, then the reader parked on its newest line. The
# rows are the transcript's own kind of element; what is measured is the list around them.
DOCK_TRANSCRIPT_JS = """async () => {
    const list = document.getElementById('transcript');
    for (let i = 0; i < 40; i += 1) {
        const row = document.createElement('li');
        row.textContent = `turn ${i}, long enough to take a line of the column and make the list scroll`;
        list.append(row);
    }
    const area = document.getElementById('scroll-area');
    area.scrollTop = area.scrollHeight;
    await new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
    return area.scrollHeight > area.clientHeight * 2;
}"""

# Where the reader is in the list: how far the newest line is below the fold, and the scroll offset.
DOCK_PLACE_JS = """async () => {
    await new Promise((settled) => requestAnimationFrame(() => requestAnimationFrame(settled)));
    const area = document.getElementById('scroll-area');
    return {gap: area.scrollHeight - area.clientHeight - area.scrollTop, top: area.scrollTop};
}"""

# The pace popover, open: what is wrong with where it is. It belongs over the list, above the dock and
# inside the column, and not over the switch or anything else in the dock.
DOCK_POPOVER_JS = """() => {
    const problems = [];
    const pop = document.getElementById('speed-popover').getBoundingClientRect();
    const dock = document.getElementById('dock'), d = dock.getBoundingClientRect(), ds = getComputedStyle(dock);
    if (pop.bottom > d.top + 0.5) problems.push(`it hangs ${(pop.bottom - d.top).toFixed(1)}px into the dock`);
    if (pop.top < 0) problems.push('it runs off the top of the window');
    if (pop.left < d.left + parseFloat(ds.paddingLeft) - 0.5 || pop.right > d.right - parseFloat(ds.paddingRight) + 0.5)
        problems.push(`it is outside the column: ${pop.left.toFixed(1)}-${pop.right.toFixed(1)}`);
    const s = document.getElementById('view-switch').getBoundingClientRect();
    const hit = document.elementFromPoint((s.left + s.right) / 2, (s.top + s.bottom) / 2);
    if (!hit || !hit.closest('#view-switch')) problems.push('the switch cannot be clicked while it is open');
    return problems;
}"""


class DockBox(TypedDict):
    """Where one control in the dock is drawn, in CSS pixels."""

    left: float
    top: float
    width: float
    height: float


class DockPicker(TypedDict):
    """A picker's width beside the width the name it shows needs."""

    text: str
    width: float
    natural: float


class DockFound(TypedDict):
    """One measurement of the dock (`DOCK_JS`)."""

    height: float
    rows: int
    visual: list[str]
    pane: list[str]
    pickers: dict[str, DockPicker]
    problems: list[str]
    boxes: dict[str, DockBox]


# One measurement of the dock as it is drawn: its height, every control in it with its box, how many
# rows they make, and what is wrong — against the column on a desk, against the tile on a phone.
DOCK_JS = """({desk, order}) => {
    const problems = [];
    const shown = (el) => !!el && el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden';
    const box = (el) => el.getBoundingClientRect();
    const dock = document.getElementById('dock'), d = box(dock), ds = getComputedStyle(dock);
    const inner = {left: d.left + parseFloat(ds.paddingLeft), right: d.right - parseFloat(ds.paddingRight)};
    const controls = [...dock.querySelectorAll('button, select')]
        .filter((el) => shown(el) && !el.closest('#prompts-tray') && !el.closest('#speed-popover'));
    const boxes = controls.map((el) => ({id: el.id, r: box(el)}));
    // A row is the controls whose feet line up: on a desk each row of the dock stands what is in it
    // on one floor, so a taller button — Talk with its note — is still on the row it sits in. (The
    // phone's rows are not asked about.)
    const foot = (r) => r.bottom;
    const rows = [];
    for (const {r} of boxes) if (!rows.some((m) => Math.abs(m - foot(r)) < 4)) rows.push(foot(r));
    rows.sort((a, b) => a - b);
    const rowOf = (r) => rows.findIndex((m) => Math.abs(m - foot(r)) < 4);
    const visual = [...boxes].sort((a, b) => rowOf(a.r) - rowOf(b.r) || a.r.left - b.r.left).map((b) => b.id);
    const pane = [...document.getElementById('control-pane').children]
        .filter((el) => shown(el) && el.matches('button'))
        .sort((a, b) => rowOf(box(a)) - rowOf(box(b)) || box(a).left - box(b).left).map((el) => el.id);
    if (document.documentElement.scrollWidth > innerWidth) problems.push(`the page is ${document.documentElement.scrollWidth}px wide in a ${innerWidth}px window`);
    // On a desk nothing may be scrolled out of the pack's sight. (A phone's pack is built to scroll,
    // and on the channel view at 412px it already hid a few pixels of the view picker before #218.)
    const pack = document.getElementById('bar-pack');
    if (desk && shown(pack) && pack.scrollWidth > pack.clientWidth + 1) problems.push(`the pack hides ${pack.scrollWidth - pack.clientWidth}px of its members past its edge`);
    const pickers = {};
    if (desk) {
        // Not on a phone: there the tile's narrow column puts Hide read over Sound, which the
        // channel view leaves unhidden, and that is the phone as it was.
        for (let i = 0; i < boxes.length; i += 1) {
            for (let j = i + 1; j < boxes.length; j += 1) {
                const a = boxes[i].r, b = boxes[j].r;
                const w = Math.min(a.right, b.right) - Math.max(a.left, b.left), h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
                if (w > 0.5 && h > 0.5) problems.push(`#${boxes[i].id} and #${boxes[j].id} overlap`);
            }
        }
        const list = ['pane-discord', 'pane-voice'].map((id) => document.getElementById(id)).find(shown);
        const column = list ? box(list) : {left: inner.left, right: inner.right};
        const note = document.getElementById('talk-note');
        for (const {id, r} of boxes) {
            const tall = id === 'talk' && shown(note) ? 64 : DESK[1];
            if (r.height < DESK[0] - 0.5 || r.height > tall + 0.5) problems.push(`#${id} is ${r.height.toFixed(1)}px tall`);
            if (r.width < DESK[0] - 0.5) problems.push(`#${id} is ${r.width.toFixed(1)}px wide`);
            if (r.left < inner.left - 0.5 || r.right > inner.right + 0.5) problems.push(`#${id} runs out of the dock's column`);
            if (r.left < column.left - 1 || r.right > column.right + 1) problems.push(`#${id} is outside the list's column`);
            if (r.top < d.top || r.bottom > d.bottom + 0.5) problems.push(`#${id} hangs out of the dock`);
            // The word level with its icon, as a desktop control's is: all but Talk over its note.
            const el = document.getElementById(id);
            const icon = el.querySelector(':scope > svg:not([hidden])');
            const word = el.querySelector(':scope > .control-label, :scope > .mini-label');
            if (icon && word && !(id === 'talk' && shown(note))) {
                const i = icon.getBoundingClientRect(), w = word.getBoundingClientRect();
                const off = (w.top + w.bottom) / 2 - (i.top + i.bottom) / 2;
                if (Math.abs(off) > 0.5) problems.push(`#${id}'s word stands ${off.toFixed(1)}px off its icon's middle`);
            }
        }
        // The switch's one place: the column's lower right corner, at the end of the dock's last row,
        // with nothing to the right of it and nothing below it.
        const flick = boxes.find(({id}) => id === 'view-switch');
        if (flick) {
            const s = flick.r;
            if (Math.abs(s.right - inner.right) > 0.5) problems.push(`the switch ends ${(inner.right - s.right).toFixed(1)}px short of the column's right edge`);
            if (Math.abs(s.bottom - (d.bottom - parseFloat(ds.paddingBottom))) > 0.5) problems.push(`the switch is not at the foot of the dock`);
            if (boxes.some(({id, r}) => id !== 'view-switch' && (r.top + r.bottom) / 2 > (s.top + s.bottom) / 2 + 4)) problems.push('a control sits below the switch');
            if (boxes.some(({id, r}) => id !== 'view-switch' && r.right > s.left + 0.5 && r.bottom > s.top + 0.5)) problems.push('a control sits right of the switch on its row');
            // Its width is fixed so that its place can be; the word in it must still be whole.
            const word = document.getElementById('view-switch-label');
            if (word.scrollWidth > word.clientWidth) problems.push(`the switch's word "${word.textContent}" is cut short`);
        }
        // Untruncated: as wide as the same select sized to the option it shows, with no cap and no shrink.
        for (const id of ['discord-channel', 'thread-select']) {
            const sel = document.getElementById(id);
            if (!shown(sel)) continue;
            const probe = document.createElement('select');
            probe.className = sel.className;
            const option = document.createElement('option');
            option.textContent = sel.selectedOptions[0] ? sel.selectedOptions[0].textContent : '';
            probe.append(option);
            probe.style.cssText = 'position:absolute;visibility:hidden;max-width:none;min-width:0;flex:none;field-sizing:content';
            sel.parentNode.append(probe);
            const natural = box(probe).width;
            probe.remove();
            pickers[id] = {text: option.textContent, width: box(sel).width, natural};
        }
    } else {
        // The thumb's targets, as the phone has always had them. The view switch is a pill drawn to
        // the strip's height rather than a target of its own, and it is left as it was.
        for (const {id, r} of boxes.filter(({id}) => id !== 'view-switch')) {
            if (r.height < 2.4 * 16 - 1) problems.push(`#${id} is ${r.height.toFixed(1)}px tall, short of a thumb`);
            if (r.width < 2.75 * 16 - 1) problems.push(`#${id} is ${r.width.toFixed(1)}px wide, narrower than a thumb`);
        }
    }
    // On a desk the pane is a row, read left to right; the phone's tile is checked by its shape.
    if (desk && order && JSON.stringify(pane) !== JSON.stringify(order)) problems.push(`the pane reads ${pane.join(', ')}`);
    return {height: d.height, rows: rows.length, visual, pane, pickers, problems,
            boxes: Object.fromEntries(boxes.map(({id, r}) => [id, {left: r.left, top: r.top, width: r.width, height: r.height}]))};
}""".replace("DESK[0]", str(DESK_CONTROL_PX[0])).replace("DESK[1]", str(DESK_CONTROL_PX[1]))

# Tab through the dock from the gear and say where focus went, and whether each stop drew a ring
# that nothing clipped: the pack scrolls sideways, and a scrolling box clips what is drawn outside it.
DOCK_FOCUS_JS = """() => {
    const el = document.activeElement;
    if (!el || !document.getElementById('dock').contains(el)) return {id: null};
    const s = getComputedStyle(el), r = el.getBoundingClientRect();
    const reach = parseFloat(s.outlineWidth) + parseFloat(s.outlineOffset);
    const problems = [];
    if (s.outlineStyle === 'none' || parseFloat(s.outlineWidth) < 2) problems.push(`#${el.id} draws no focus ring`);
    const clip = el.closest('#bar-pack');
    if (clip) {
        const c = clip.getBoundingClientRect();
        if (r.top - reach < c.top - 0.5 || r.bottom + reach > c.bottom + 0.5 || r.left - reach < c.left - 0.5
            || r.right + reach > c.right + 0.5) problems.push(`#${el.id}'s ring is clipped by the pack`);
    }
    if (r.top - reach < 0 || r.bottom + reach > innerHeight || r.left - reach < 0 || r.right + reach > innerWidth)
        problems.push(`#${el.id}'s ring runs off the window`);
    return {id: el.id, visible: el.matches(':focus-visible'), problems};
}"""


def larger_root_font(page: Page, root_percent: int) -> None:
    """Stand in for a phone reader's larger system font: every rem on the page follows the root's."""
    if root_percent != 100:
        page.add_init_script(
            "document.addEventListener('DOMContentLoaded', () => {"
            " const larger = document.createElement('style');"
            f" larger.textContent = 'html {{ font-size: {root_percent}% !important; }}';"
            " document.head.append(larger); });")


def main() -> int:
    args = arguments()
    try:
        from playwright.sync_api import sync_playwright
    except ImportError as error:
        raise SystemExit(
            "Python Playwright is required: python3 -m pip install --user playwright && "
            "python3 -m playwright install chromium"
        ) from error
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)

    with sync_playwright() as playwright:
        windows_walk(playwright.chromium, args)
        print("desktop at 1280x800, two windows of one profile: each reloaded on its own channel, not the"
              " one the other chose last, and a third window opened on the channel chosen last")
        for label, width, height, mobile in DOCK_SIZES:
            heights = dock_walk(playwright.chromium, args, label, width, height, mobile)
            if mobile:
                print(f"{label} at {width}x{height}, dark: the dock the phone had — its bar and its tile of"
                      f" large buttons at their heights and widths, every control a thumb's size: {heights}")
            else:
                print(f"{label} at {width}x{height}, dark: the dock one row of {DESK_CONTROL_PX[0]}-"
                      f"{DESK_CONTROL_PX[1]}px controls where it fits and two where it does not, inside the"
                      " column and overlapping nothing, the pickers wide enough for a thirty-character"
                      " channel name with a thread open beside it, the pane in the order the phone shows,"
                      " every word level with its icon, the switch whole and in the column's lower right"
                      " corner in every state with a second click in the same spot switching back, the pace"
                      " popover over the list and clear of the switch, a reader on the newest turn kept"
                      " there as the dock grows a row and one up the history left alone,"
                      f" and every Tab stop ringed and unclipped: {heights}")
        for label, width, height, mobile in PROFILES:
            pin_walk(playwright.chromium, args, label, width, height, mobile)
            print(f"{label} at {width}x{height}: a pin made elsewhere shown as a legible Pinned chip and an"
                  " edge, the ⋯ menus of a pinned and an unpinned row as Copy text, Pin and the captioned"
                  " two-item read group at full size and on the screen, Pin shown at once and stored, and"
                  " the Pinned filter beside the glass with a 44px target clear of the glass and the field,"
                  " showing the pins and folded away with the bar")
            links_walk(playwright.chromium, args, label, width, height, mobile)
            print(f"{label} at {width}x{height}: the search glass at least {GLASS_MIN_PX}px across; Links and"
                  " Pinned beside it on its line, each a 44px target clear of the field and the glass, the"
                  " field at least 140px wide beside a count; Links showing only the rows with links, each"
                  " link real, on its own line and a 44px target, a long one ellipsised; a real"
                  f" {'tap' if mobile else 'click'} on a link opening a new tab without folding its row; Links"
                  " folded away with the bar, and alone beside the glass over the call view")
            browser_version = walk(playwright.chromium, args, label, width, height, mobile)
            print(f"{browser_version} {label} at {width}x{height}: All by default, a reload reopened on the"
                  " channel, its view and the reader's message, snapshot drawn before the network,"
                  " one newest-page read of the"
                  " view on screen, local view switches, offline and failed states, the pill"
                  " clear of the tabs and the header with #scroll-area unmoved, a scrolled reader"
                  " held in place as it and the error panel come and go, the jump to the newest message"
                  " at the lower left of the chip row whenever the list is scrolled back and gone at the"
                  " newest line, a real tap on it landing there, the same when the list resizes under a"
                  " still reader, the search glass on the pill's"
                  " line with no header strip and a 44px target, the search bar opened by a real tap and"
                  " folded with #scroll-area unmoved, Settings' title bar,"
                  f"{' a pull up past the newest line refreshes in place,' if mobile else ''} no Cache Storage or"
                  " service worker, a long thread title ellipsised within the viewport, the thread"
                  " composer clear of every floating chip, sign-out clears,"
                  " a read-scope token reads with no refused request or console error")
            print(f"{reply_walk(playwright.chromium, args, label, width, height, mobile)} {label} at"
                  f" {width}x{height}: every reply's arrow a 44px target in the gutter left of its box, clear"
                  " of the row's text and buttons, drawn in the accent at 3:1 or better on every row"
                  " that recedes, at the default type size, at 150% and in the dark scheme; a real"
                  f" {'tap' if mobile else 'click'} on it brought the message it answers under the floating"
                  " line, lit, without folding the reply or, in reading mode, reading it aloud; Back to reply"
                  " brought the reply back")
            rows = tile_walk(playwright.chromium, args, label, width, height, mobile)
            print(f"{label} at {width}x{height}, dark: {rows} channel rows each drawn with four sides; the"
                  " coding agent's tiles square, their left and right sides the width, style and colour of"
                  " their top or a summary's wider bar on the left, "
                  f"{'from edge to edge of the screen' if mobile else 'inside the reading column'}, beside the"
                  " owner's and a third party's rows and in every state that redraws a border; nothing on"
                  " the page scrolling sideways")
            coalesce_walk(playwright.chromium, args, label, width, height, mobile)
            print(f"{label} at {width}x{height}: a reply directly under what it answers draws the arrow whose head"
                  " meets that row, at its foot or its side, within 1px, a press on that head its arrow's even over"
                  f" another arrow, a real {'tap' if mobile else 'click'} there jumping to the row it meets and two"
                  " both that arrow's, gathering its conversation under the conversation's head; and a reply to a"
                  " message elsewhere the one leaving its box's middle at 45 degrees, at the default type size and at"
                  f" 150%; a real {'tap' if mobile else 'click'} on N replies gathered the replies, a reply to a reply"
                  " among them, under their root, the root"
                  " unmoved, one bridge reaching every reply's left edge from the root's foot, through an opened"
                  " reply and 150% type, its X a 44px target that put them back with the root unmoved; two"
                  f" {'taps' if mobile else 'clicks'} on an arrow gathered them with the reply kept in place")
        for label, width, height, root_percent in MENU_SCALES:
            menu_walk(playwright.chromium, args, label, width, height, root_percent)
            print(f"{label} at {width}x{height}, root font {root_percent}%: every row's ⋯ menu, the pinned"
                  " row's in the Pinned filter too, on the screen with full-size items a tap reaches, in"
                  " both provider shapes, and Unpin tapped for real")
        for label, width, height, root_percent in BAR_SCALES:
            text = links_bar_walk(playwright.chromium, args, label, width, height, root_percent)
            print(f"{label} at {width}x{height}, root font {root_percent}%: the open bar with Links on and its"
                  f" count up — the glass at least {GLASS_MIN_PX}px, Links and Pinned 44px targets beside it,"
                  f" the field leaving {text:.0f}px for its text (at least {FIELD_TEXT_MIN_PX}) — every"
                  " link on its own 44px line, clear of the bar, and the six kinds of link 44px targets"
                  " under Links, inside the screen")
        for label, width, height, mobile, root_percent in KINDS_SIZES:
            placed = kinds_walk(playwright.chromium, args, label, width, height, mobile, root_percent)
            print(f"{label} at {width}x{height}, root font {root_percent}%: PRs, Commits, Actions, Issues, Files"
                  f" and Other under Links, numbers adding up to every link, {placed} under it over the channel"
                  " and right-aligned under the bar's end over the call, inside the screen, each a 44px target"
                  " clear of the bar, the pill and the first row, legible on and off; a real tap or Tab and Enter"
                  " on each took its links out of their rows, and a row with nothing left went, Other keeping the"
                  " documents; the choice outlived a reload, and the glass took the row away")
        for label, width, height, root_percent in CROWDED_KINDS_SIZES:
            span = crowded_kinds_walk(playwright.chromium, args, label, width, height, root_percent)
            print(f"{label} at {width}x{height}, root font {root_percent}%: the six kinds each reading 99+,"
                  f" {span:.0f}px of buttons scrolling inside the bar, under Links and inside the screen, each a"
                  " 44px target; a real swipe brought Other into view and a real tap turned it off")
        for label, width, height, root_percent, mode in EMPTY_SIZES:
            top = empty_walk(playwright.chromium, args, label, width, height, root_percent, mode)
            print(f"{label} at {width}x{height}, root font {root_percent}%: a list emptied ({mode}) from its"
                  " newest line says why wholly on the screen, below the bar and any kinds of link under it,"
                  f" covered by nothing and with no jump offered past it, the list at {top:.0f}px; undone, the"
                  " rows came back at the newest line")
    return 0


def walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
         mobile: bool) -> str:
    """One reader, start to finish, against a fresh fake API and a fresh browser profile."""
    api = FakeApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    platform = "Linux; Android 15; Pixel 7" if mobile else "X11; Linux x86_64"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                user_agent=(
                    f"Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko)"
                    f" Chrome/151.0.0.0 {'Mobile ' if mobile else ''}Safari/537.36"
                ),
                viewport={"width": width, "height": height},
                device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile,
                has_touch=mobile,
            )
            page = context.pages[0]
            # Time flows as normal; `#32`'s in-place step jumps it past the channel's poll.
            page.clock.install()
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            # Everything the browser itself calls an error, and every refused answer: a 4xx logs a
            # console error however the page handles it, so the only fix is not to ask.
            console_errors: list[str] = []
            page.on("console", lambda entry: console_errors.append(entry.text) if entry.type == "error" else None)
            refused: list[str] = []
            page.on("response", lambda answer: refused.append(
                f"{answer.status} {answer.request.method} {urlsplit(answer.url).path}")
                if answer.status >= 400 else None)

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-{name}.png"))

            def rows() -> list[str]:
                return [str(row) for row in page.eval_on_selector_all(
                    "#discord-log > li[data-id]", "items => items.map(i => i.getAttribute('data-id'))")]

            def wait_rows(expected: list[str], why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and rows() != expected:
                    page.wait_for_timeout(50)
                check(rows() == expected, f"{why}: showing {rows()}, expected {expected}")

            def pill() -> str:
                return str(page.evaluate(
                    "() => { const p = document.getElementById('channel-freshness');"
                    " return p.hidden ? '' : p.textContent; }"))

            def current(text: str) -> bool:
                # A current list says so with the time of its last read; blank means unknown.
                return re.fullmatch(r"(Live · u|U)pdated \d{2}:\d{2}", text) is not None

            def view(name: str) -> None:
                page.evaluate(f"() => document.getElementById('channel-view-{name}').click()")

            def reopened(why: str) -> None:
                """`#189 restore-ui-state`: a reload of a page left on the channel comes back on it."""
                shown = page.evaluate("() => !document.getElementById('pane-discord').hidden")
                check(bool(shown), f"{label}: {why}: the reload did not reopen on the channel")

            def failing(route: Route) -> None:
                route.fulfill(status=502, content_type="application/json",
                              body=json.dumps({"error": "discord_error", "detail": "the provider is down"}))

            def timeline_read(address: str) -> bool:
                return "/timeline" in address

            def wait_pill(prefix: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not (pill().startswith(prefix) if prefix else current(pill())):
                    page.wait_for_timeout(50)
                check(pill().startswith(prefix) if prefix else current(pill()), f"{label}, {why}: pill {pill()!r}")

            def poll_channel(fail: bool) -> None:
                """The channel's own re-read, brought forward on the page clock rather than awaited."""
                if fail:
                    page.route(timeline_read, failing)
                page.clock.fast_forward(60_000)
                if fail:
                    wait_pill("Refresh failed", "the in-place refresh did not fail")
                    page.unroute(timeline_read, failing)
                else:
                    wait_pill("", "the in-place refresh did not recover")
                page.wait_for_timeout(100)

            def place(where: str) -> dict[str, float]:
                found = page.evaluate(PLACE_JS, where)
                return {key: float(found[key]) for key in ("row", "gap", "top", "bottom", "banner")}

            def geometry(state: str, name: str) -> str:
                # At the head of the list, where the header is: a pill over row 40 of 90 is the
                # accepted cost of an overlay, a pill over the header is `#32 freshness-pill-overlap`.
                found = page.evaluate(GEOMETRY_JS)
                shot(name)
                problems = [str(problem) for problem in found["problems"]]
                check(not problems, f"{label}, {state}: {problems}")
                check(str(found["area"]) == str(found["bare"]),
                      f"{label}, {state}: #scroll-area is {found['area']}, {found['bare']} without the pill")
                check(bool(found["picker"]), f"{label}, {state}: the bar's thread picker was not on screen")
                return str(found["area"])

            def wait_jump(up: bool) -> None:
                """`#207 scrollback-jump`: until the jump is up (or gone), for ten seconds at most.

                Waited for rather than given two frames. The page decides the jump on the list's scroll
                event, which the browser dispatches in its next RENDERING frame, and a loaded host can
                hold that frame back for half a second; `page.clock` has made requestAnimationFrame a
                16ms timer, so two frames counted in the page pass whether one was drawn or not. A jump
                that is stuck, rather than late, still fails here: ten seconds later.
                """
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and bool(page.evaluate(JUMP_UP_JS)) != up:
                    page.wait_for_timeout(50)

            def wait_room() -> None:
                """Until the room under the composer has caught up with the chip row, ten seconds at most."""
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(ROOM_SETTLED_JS):
                    page.wait_for_timeout(50)

            def park_still(gap: int) -> None:
                """Park the reader `gap` px short of the end, and again until the end stays where it is.

                The room under the composer follows the chip row a frame late (ROOM_SETTLED_JS), and a jump
                that has just come or gone has just changed the row, so the end of the list can move after
                the reader was put somewhere relative to it. Ten seconds at most.
                """
                deadline = time.monotonic() + 10
                while True:
                    page.evaluate(PARK_JS, gap)
                    wait_room()
                    if abs(float(page.evaluate(JUMP_JS)["gap"]) - gap) <= 1 or time.monotonic() > deadline:
                        return

            # 1. Sign in and open the channel: in All, the default where the provider has threads
            # (`#189 restore-ui-state`); then Threads and Main. The call view comes up first: its
            # glass floats over the transcript with no header strip above it (`#197`).
            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            alone = page.evaluate(GLASS_ALONE_JS)
            shot("1-call-view-glass")
            check(not alone["problems"], f"{label}, the call view: {alone['problems']}")
            page.click("#view-switch")
            wait_rows(["200", "201", "202"], "All after sign-in")
            check(page.evaluate("() => document.getElementById('thread-select').value") == "flat",
                  f"{label}: a channel with threads did not open in All")
            view("threads")
            page.wait_for_selector("#thread-list > li", timeout=10_000)
            view("main")
            wait_rows(["200", "201"], "Main after sign-in")
            view("flat")
            wait_rows(["200", "201", "202"], "All again")
            saved = json.loads(page.evaluate(f"() => localStorage.getItem({json.dumps(CACHE_KEY)})") or "{}")
            scope = saved.get("scopes", {}).get(CHANNEL["id"], {})
            check(sorted(scope.get("views", {})) == ["flat", "main", "threads"],
                  f"the snapshot did not cover each view read: {sorted(scope.get('views', {}))}")
            shot("1-signed-in")
            baseline = str(page.evaluate(GEOMETRY_JS)["area"])

            # 2. Reload with the history read held: the page reopens on the channel, in All, and the
            # saved rows draw before any answer. The read goes once /client-config has answered and
            # the live stream has attached, and the pill says it is refreshing from then.
            api.timeline_gate.clear()
            with api.lock:
                api.requests.clear()
                api.messages.append(message(3, "posted while the page was closed"))
            page.reload(wait_until="load")
            reopened("the reload with the read held")
            check(page.evaluate("() => document.getElementById('thread-select').value") == "flat",
                  f"{label}: the reload did not reopen in All")
            wait_rows(["200", "201", "202"], "the snapshot was not drawn before the network answered")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and "refreshing" not in pill():
                page.wait_for_timeout(50)
            text = pill()
            check(text.startswith("Saved ") and "refreshing" in text, f"pill while refreshing: {text!r}")
            check(geometry("refreshing", "2-saved-before-network") == baseline,
                  f"{label}: #scroll-area is not the size it was before the reload")

            # 3. One bounded newest page for the view on screen — All — merges without duplicating.
            api.timeline_gate.set()
            wait_rows(["200", "201", "202", "203"], "the refresh did not merge")
            reads = api.reads()
            check(len(reads) == 1, f"a cold start made {len(reads)} history reads: {reads}")
            check("view=flat" in reads[0] and "before=" not in reads[0],
                  f"the cold-start read was not the newest page of All: {reads[0]}")
            # After the token was proved and the live stream answered: a read begun before the
            # attach cannot answer for the stream's replay tail (`#43 replay-burst-double-read`).
            with api.lock:
                order = [r for r in api.requests if "/client-config" in r or "/stream" in r or r in reads]
            kinds = ["config" if "/client-config" in r else "stream" if "/stream" in r else "read" for r in order]
            check(kinds[:3] == ["config", "stream", "read"],
                  f"the cold-start read did not wait for sign-in and the stream: {order}")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not current(pill()):
                page.wait_for_timeout(50)
            check(current(pill()), f"a refreshed view still says {pill()!r}")
            check(str(page.evaluate(GEOMETRY_JS)["area"]) == baseline,
                  f"{label}: #scroll-area changed size when the pill turned current")
            shot("3-merged")

            # 4. Switching among views already covered is local. It ends on Main, the reader's
            # choice, which the reloads below reopen on.
            view("threads")
            page.wait_for_selector("#thread-list > li", timeout=5_000)
            view("main")
            wait_rows(["200", "201", "203"], "Main after the refresh")
            view("flat")
            wait_rows(["200", "201", "202", "203"], "All after switching back")
            view("main")
            wait_rows(["200", "201", "203"], "Main again")
            check(len(api.reads()) == 1, f"switching views read history again: {api.reads()}")
            shot("4-switched-locally")

            # 4a. `#197 floating-search`. Settings is still a title bar with a way back, and leaving
            # it leaves no strip behind. A real tap BELOW the disc — outside it, inside its 44px
            # square — opens the search as a bar over the top of the list; the bar filters; the
            # glass folds it again and the list is the box it was.
            page.click("#open-settings")
            title_bar = page.evaluate(TITLE_BAR_JS)
            shot("4a-settings-title-bar")
            check(bool(title_bar["shown"]) and float(title_bar["height"]) > 30
                  and title_bar["title"] == "Settings" and bool(title_bar["back"])
                  and not title_bar["glass"], f"{label}, Settings lost its title bar: {title_bar}")
            page.click("#close-settings")
            check(not page.evaluate(TITLE_BAR_JS)["shown"],
                  f"{label}: coming back from Settings left a header strip over the channel")
            centre = page.evaluate("() => { const b = document.getElementById('search-toggle')"
                                   ".getBoundingClientRect(); return [b.left + b.width / 2, b.bottom + 5]; }")
            if mobile:
                page.touchscreen.tap(float(centre[0]), float(centre[1]))
            else:
                page.mouse.click(float(centre[0]), float(centre[1]))
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and page.evaluate("() => document.getElementById('search-field').hidden"):
                page.wait_for_timeout(50)
            opened = page.evaluate(SEARCH_OPEN_JS, "")
            shot("4a-search-open")
            check(not opened["problems"], f"{label}, a tap 5px below the glass's disc: {opened['problems']}")
            page.fill("#search-field", "thread root")
            found = page.evaluate(SEARCH_OPEN_JS, "thread root")
            shot("4a-search-filtered")
            check(not found["problems"], f"{label}, the search filtered: {found['problems']}")
            check(found["rows"] == ["201"] and found["count"] == "1 of 3 loaded",
                  f"{label}: the floating search left {found['rows']} saying {found['count']!r}")
            page.click("#search-toggle")
            check(bool(page.evaluate("() => document.getElementById('search-field').hidden")),
                  f"{label}: the glass did not fold the search bar")
            wait_rows(["200", "201", "203"], "folding the search did not put every row back")
            check(geometry("search folded", "4a-search-folded") == baseline,
                  f"{label}: #scroll-area changed size across opening and folding the search")

            # 4b. A failed refresh IN PLACE, over a list long enough to scroll: the pill's room is made
            # at the head of the list and the error panel takes a row above it, and both go again
            # when the next read succeeds. Browser scroll anchoring holds for neither, so the page
            # must: the row being read stays where it is on the screen, and a reader at the newest
            # line stays there (`#32 freshness-pill-overlap`, `#33 error-banner-scroll-shift`). At the
            # very top the header moves down out from under them instead. Tall rows, so that three
            # of them overflow.
            page.add_style_tag(content="#discord-log > li[data-id] { min-height: 70vh; }")
            for where in ("middle", "newest", "top"):
                before = place(where)
                parked = {"middle": 0 < before["top"] and before["gap"] > 100,
                          "newest": before["gap"] <= 2, "top": before["top"] == 0}[where]
                check(parked, f"{label}: could not park the reader at the {where}: {before}")
                # A failed POLL is carried by the pill alone since `#195 send-resilience`: nobody asked
                # for that read, so the error panel stays down and only the pill's room is made at the
                # head of the list. Twice, so a second failure after a recovery is held the same way.
                for step in ("fail", "recover", "fail", "recover"):
                    poll_channel(step == "fail")
                    after = place("")
                    state = f"{label}, reader at the {where}: {step}"
                    check(after["banner"] == 0.0, f"{state} but a background refresh raised the error panel: {after}")
                    check(abs(after["bottom"] - before["bottom"]) < 1, f"{state} and moved the list's foot: {after}")
                    if where == "middle":
                        moved = after["row"] - before["row"]
                        # Under 2px a step: a refresh makes several position fixes (its loading line
                        # arriving, the pill's room, the panel, the loading line going), each landing
                        # on a whole pixel. The defects were the pill's height, ~32px, and the
                        # panel's, 74-89px.
                        check(abs(moved) < 2, f"{state} and moved the row being read by {moved:.1f}px: {after}")
                    elif where == "newest":
                        check(after["gap"] <= 2, f"{state} and left the newest line {after['gap']:.1f}px below")
                    else:
                        check(after["top"] == 0, f"{state} and scrolled the header away: {after}")
                        if step == "fail":
                            geometry("in place at the top", "4b-top")
                    before = after
            shot("4b-in-place")

            # 4b2. `#207 scrollback-jump`, in the dark theme the owner reads in. At the newest line there
            # is no jump; halfway up the channel it is at the lower left of the chip row, legible, hit at
            # 44px and clear of everything around it — alone, beside every other chip forced up, and
            # beside them at a large system font, where the row has to wrap and the others go up a line.
            # Then a real tap a few pixels ABOVE the drawn chip lands on the newest message, reads
            # nothing, starts no pull, and the jump is gone. Each state is waited for (`wait_jump`).
            page.emulate_media(color_scheme="dark")
            page.evaluate(PARK_JS, 0)
            wait_jump(False)
            resting = page.evaluate(JUMP_JS)
            check(float(resting["gap"]) <= 2, f"{label}: could not park the reader at the newest line: {resting}")
            check(not resting["shown"], f"{label}: the jump to the newest message is up at the newest line: {resting}")
            page.evaluate(PARK_JS, "middle")
            wait_jump(True)
            back = page.evaluate(JUMP_JS)
            shot("4b2-jump-scrolled-back")
            check(bool(back["shown"]) and float(back["gap"]) > 100,
                  f"{label}: halfway up the channel there is no jump to the newest message: {back}")
            check(back["label"] == "Newest", f"{label}: the jump says {back['label']!r} with nothing new below")
            check(not back["problems"], f"{label}, the jump halfway up the channel: {back['problems']}")
            crowd = page.add_style_tag(content="#scroll-tools .chip[hidden] { display: inline-flex !important; }")
            for size in ("100%", "150%"):
                font = page.add_style_tag(content=f"html {{ font-size: {size} !important; }}")
                crowded = page.evaluate(JUMP_JS)
                shot(f"4b2-jump-beside-every-chip-{size.rstrip('%')}")
                lines = int(crowded["lines"])
                check(not crowded["problems"],
                      f"{label}, the jump beside every chip at {size} type, {lines} lines: {crowded['problems']}")
                # On a phone the full row does not fit one line, so this IS the overflow case.
                check(lines >= 2 or not mobile, f"{label}: every chip at {size} type fit one line, so nothing wrapped")
                font.evaluate("element => element.remove()")
            crowd.evaluate("element => element.remove()")
            ready = page.evaluate(JUMP_JS)
            check(bool(ready["shown"]) and not ready["problems"], f"{label}: the jump before the tap: {ready}")
            reads_before = len(api.reads())
            tap_x, tap_y = float(ready["centre"][0]), float(ready["top"]) - 4
            if mobile:
                page.touchscreen.tap(tap_x, tap_y)
            else:
                page.mouse.click(tap_x, tap_y)
            wait_jump(False)
            landed = page.evaluate(JUMP_JS)
            newest = page.evaluate(NEWEST_ROW_JS)
            shot("4b2-jump-tapped")
            check(float(landed["gap"]) <= 2,
                  f"{label}: a tap 4px above the jump left the newest line {float(landed['gap']):.1f}px below")
            check(bool(newest["visible"]), f"{label}: after the tap the end of the newest message is not on screen: {newest}")
            check(not landed["shown"], f"{label}: the jump is still up at the newest line: {landed}")
            check(len(api.reads()) == reads_before, f"{label}: the tap read the channel: {api.reads()[reads_before:]}")
            check(bool(page.evaluate("() => document.getElementById('pull-refresh').hidden")),
                  f"{label}: the tap started the pull's affordance")

            # ...and decided again when the list changes size under a reader who has NOT scrolled, which
            # no scroll event reports — so each change is made with the list's scroll events held back
            # from the page, and the scroll listener cannot be what answers it. The newest message growing
            # 200px below a reader on the newest line puts them up the history: the jump comes. Parked
            # 40px up with it showing, the newest message then shrinking by 30px puts them on the newest
            # line: it goes.
            # Taller than the row is drawn now, by exactly `px`: a min-height stated in vh would grow it
            # by less whenever its words already make it taller than that.
            drawn = float(page.evaluate(
                "() => [...document.querySelectorAll('#discord-log > li[data-id]')].pop().getBoundingClientRect().height"))

            def newest_row_taller(px: int) -> str:
                return (f"#discord-log > li[data-id]:last-child {{ box-sizing: border-box !important;"
                        f" min-height: {drawn + px}px !important; }}")
            # On the newest line, with the room under the composer settled: it can still be giving back the
            # height of the crowded chip rows above, and that moves the end the reader is measured against.
            park_still(0)
            page.evaluate("() => { window.__holdScrolls = true; }")
            grown = page.add_style_tag(content=newest_row_taller(200))
            wait_jump(True)
            below = page.evaluate(JUMP_JS)
            page.evaluate("() => { window.__holdScrolls = false; }")
            check(bool(below["shown"]) and float(below["gap"]) > 100,
                  f"{label}: the newest message grew 200px below a reader on the newest line, and no jump came: {below}")
            # Down from halfway up, so the jump is up by the scroll listener's own decision.
            page.evaluate(PARK_JS, "middle")
            wait_jump(True)
            park_still(40)
            parked = page.evaluate(JUMP_JS)
            check(bool(parked["shown"]) and abs(float(parked["gap"]) - 40) <= 1,
                  f"{label}: come down to 40px short of the end from halfway up, the jump is not still up: {parked}")
            page.evaluate("() => { window.__holdScrolls = true; }")
            grown.evaluate(f"(element) => {{ element.textContent = {json.dumps(newest_row_taller(170))}; }}")
            wait_jump(False)
            met = page.evaluate(JUMP_JS)
            page.evaluate("() => { window.__holdScrolls = false; }")
            check(not met["shown"] and float(met["gap"]) <= 24,
                  f"{label}: the newest message shrank 30px under a reader 40px up, onto the newest line, and the"
                  f" jump stayed: {met}")
            grown.evaluate("element => element.remove()")
            page.evaluate(PARK_JS, 0)
            wait_jump(False)
            page.emulate_media(color_scheme="light")

            # 4c. `#188 pull-refresh-bottom`, with a real finger: parked at the newest line, a drag UP
            # past the end of the list reads the channel once and says so at the foot of the list,
            # in this same page — no reload — and a drag short of the threshold reads nothing. The
            # touches go through Chromium's own input pipeline, so this is also the check that the
            # passive listeners still hear the overscroll at the end of a list that cannot scroll.
            if mobile:
                cdp = context.new_cdp_session(page)

                def drag_up(travel: float) -> None:
                    """One finger, landed on a message low in the list and drawn up, then lifted."""
                    box = page.evaluate("() => { const r = document.getElementById('scroll-area')"
                                        ".getBoundingClientRect(); return [r.left + r.width / 2,"
                                        " r.top + r.height * 0.6]; }")
                    x, y = float(box[0]), float(box[1])
                    steps = 12
                    cdp.send("Input.dispatchTouchEvent", {"type": "touchStart", "touchPoints": [{"x": x, "y": y}]})
                    for step in range(1, steps + 1):
                        page.wait_for_timeout(16)
                        cdp.send("Input.dispatchTouchEvent", {
                            "type": "touchMove", "touchPoints": [{"x": x, "y": y - travel * step / steps}]})
                    page.wait_for_timeout(16)
                    cdp.send("Input.dispatchTouchEvent", {"type": "touchEnd", "touchPoints": []})

                def affordance() -> tuple[str, str, str]:
                    found = page.evaluate("() => { const p = document.getElementById('pull-refresh');"
                                          " return [p.hidden ? '' : p.textContent,"
                                          " p.getAttribute('data-state'), p.getAttribute('data-edge')]; }")
                    return str(found[0]), str(found[1]), str(found[2])

                resting = place("newest")
                check(resting["gap"] <= 2, f"{label}: could not park the reader at the newest line: {resting}")
                page.evaluate("() => { window.pullRefreshPageMark = true; }")
                # The channel's own poll — or any other timer that re-reads it, such as the live
                # stream re-attaching — would be indistinguishable from the pull's read, so the page
                # clock stands still while the finger is down; the fetch itself needs no timer. Then
                # a moment for anything those timers already put on the wire to land before counting.
                page.clock.pause_at(int(page.evaluate("() => Date.now()")) + 1000)
                page.wait_for_timeout(500)
                with api.lock:
                    seen = len(api.requests)
                reads_before = len(api.reads())
                drag_up(30)
                page.wait_for_timeout(400)
                with api.lock:
                    during = api.requests[seen:]
                check(len(api.reads()) == reads_before,
                      f"{label}: a 30px drag at the newest line read the channel; requests since: {during}")
                check(affordance()[0] == "", f"{label}: a short drag left the affordance up: {affordance()}")
                drag_up(160)
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not affordance()[0].startswith("Updated"):
                    page.wait_for_timeout(50)
                shown = affordance()
                shot("4c-pulled-up")
                fresh = api.reads()[reads_before:]
                check(len(fresh) == 1 and "before=" not in fresh[0],
                      f"{label}: a pull up past the newest line made {len(fresh)} reads: {fresh}")
                check(re.fullmatch(r"Updated \d{2}:\d{2} · nothing new", shown[0]) is not None
                      and shown[2] == "end", f"{label}: the foot of the list said {shown}")
                check(bool(page.evaluate("() => window.pullRefreshPageMark === true")),
                      f"{label}: the pull up reloaded the page instead of re-reading the channel")
                page.clock.resume()
                # After the resume: `place` waits on animation frames, which the paused clock holds.
                check(place("")["gap"] <= 2, f"{label}: the refresh moved the reader off the newest line")
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and affordance()[0]:
                    page.wait_for_timeout(50)
                check(affordance()[0] == "", f"{label}: the pull's result never went: {affordance()}")
                cdp.detach()

            # 4d. `#189 restore-ui-state`. A reader parked mid-list, whose app goes away, comes back
            # on the same message. The tall rows are served in the stylesheet from here on, so every
            # later load lays them out before the page's script runs, as a phone does; the reload
            # is step 5's, with the API unreachable, so only the device can put the reader back.
            def tall_rows(route: Route) -> None:
                served = route.fetch()
                route.fulfill(response=served,
                              body=served.text() + "\n#discord-log > li[data-id] { min-height: 70vh; }\n")

            page.route("**/voice.css", tall_rows)
            reading = place("middle")
            check(reading["top"] > 0 and reading["gap"] > 100, f"{label}: could not park the reader mid-list: {reading}")

            # 5. The API unreachable: the rows stay, and the pill says they are old.
            page.route("**/api/**", lambda route: route.abort("internetdisconnected"))
            page.reload(wait_until="load")
            reopened("offline")
            wait_rows(["200", "201", "203"], "the snapshot was not kept while offline")
            # `#205 channel-view-memory`: Main came back as this channel's own choice, kept in the
            # browser's storage with the channel it was chosen in.
            record = json.loads(page.evaluate(f"() => localStorage.getItem({json.dumps(UI_STATE_KEY)})") or "{}")
            chosen = [(entry.get("channel"), entry.get("channelView")) for entry in record.get("channels", [])]
            check(record.get("v") == 2 and chosen == [(CHANNEL["id"], "main")],
                  f"{label}: Main was not kept as the channel's choice: v{record.get('v')} {chosen}")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not pill().startswith("Offline"):
                page.wait_for_timeout(50)
            check(pill().startswith("Offline · showing messages saved "), f"offline pill: {pill()!r}")
            back = place("")
            check(abs(back["row"] - reading["row"]) < 2,
                  f"{label}: the reopen moved the reader's message by {back['row'] - reading['row']:.1f}px: {back}")
            geometry("offline", "5-offline")
            page.unroute("**/api/**")

            # 5b. The API reachable but failing: the rows stay, and the pill says the refresh failed.
            page.route(timeline_read, failing)
            page.reload(wait_until="load")
            reopened("a failing refresh")
            wait_rows(["200", "201", "203"], "the snapshot was not kept after a failed refresh")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not pill().startswith("Refresh failed"):
                page.wait_for_timeout(50)
            check(pill().startswith("Refresh failed · showing messages from "), f"failed pill: {pill()!r}")
            geometry("failed", "5b-failed")
            page.unroute(timeline_read, failing)

            # 6. Nothing private is in Cache Storage, and no service worker holds one.
            cache_names = page.evaluate("() => caches.keys()")
            check(cache_names == [], f"Cache Storage holds {cache_names}")
            workers = page.evaluate("() => navigator.serviceWorker.getRegistrations().then(r => r.length)")
            check(workers == 0, f"{workers} service workers are registered")

            # 6b. A thread named longer than the screen: its title is ellipsised, not the page widened.
            view("threads")
            # The card's replies button goes into the thread; the card itself only highlights (`#277
            # thread-card-focus`).
            page.click("#thread-list > li button.thread-card-open")
            page.wait_for_selector("#thread-heading:not([hidden])", timeout=5_000)
            wait_rows(["201", "202"], "the long-titled thread did not open")
            found = page.evaluate(WIDTH_JS)
            shot("6b-long-thread-title")
            check(not found["problems"], f"{label}, a long-titled thread: {found['problems']}")
            check(bool(found["clipped"]) or not mobile, f"{label}: the long title was not ellipsised: {found}")
            heading = page.evaluate(HEADING_JS)
            check(not heading, f"{label}, a thread's heading and the floating line: {heading}")

            # 6c. Every floating chip up at once, over that thread scrolled to its end: none of them
            # covers the reply composer, which keeps their measured height as room under itself
            # (`#194 thread-picker-polish`). The rows are still 70vh tall from 4b, so the composer
            # really is at the foot of the list, where the chips float.
            forced = page.add_style_tag(content="#scroll-tools .chip[hidden] { display: inline-flex !important; }")
            covered = [str(problem) for problem in page.evaluate(COMPOSER_JS)]
            shot("6c-composer-clear-of-chips")
            check(not covered, f"{label}, the thread composer under the floating chips: {covered}")
            forced.evaluate("element => element.remove()")
            page.click("#thread-back")

            # 7. Signing out takes the rows off the screen and off the device.
            page.evaluate("() => document.getElementById('forget-token').click()")
            check(page.evaluate(f"() => localStorage.getItem({json.dumps(CACHE_KEY)})") is None,
                  "signing out left the snapshot on the device")
            check(rows() == [], f"signing out left rows on the screen: {rows()}")
            shot("7-signed-out")

            # 8. A fresh page and a READ-scope token: it reads the channel and asks for nothing it
            # cannot have. Reloaded first, so the stored record has not been looked for yet. Signing
            # out forgot where the reader was, so this opens on the call view, and the channel in All.
            page.reload(wait_until="load")
            console_errors.clear()
            refused.clear()
            page.fill("#api-token", READ_TOKEN)
            page.click("#save-token")
            page.click("#view-switch")
            wait_rows(["200", "201", "202", "203"], "a read-scope token did not read the channel")
            def storage_state() -> str:
                return str(page.evaluate("() => document.getElementById('storage-state').textContent"))

            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not refused and "write-scope token" not in storage_state():
                page.wait_for_timeout(50)
            page.wait_for_timeout(300)
            shot("8-read-scope")
            check(not refused, f"{label}, read-scope token: the page was refused {refused}")
            check(not console_errors, f"{label}, read-scope token: console errors {console_errors}")
            check("write-scope token" in storage_state(),
                  f"{label}, read-scope token: Settings says {storage_state()!r} about the stored record")
            check(bool(page.evaluate("() => document.getElementById('error-wrap').hidden")),
                  f"{label}, read-scope token: the error panel is up")

            desktop = bool(page.evaluate("() => matchMedia('(min-width: 900px) and (pointer: fine)').matches"))
            check(desktop != mobile, f"{label}: the page chose the wrong layout regime")
            check(not errors, f"the page threw: {errors}")
            browser_version = context.browser.version if context.browser else "Chromium"
            context.close()
        return browser_version
    finally:
        api.stopping.set()
        api.timeline_gate.set()
        server.shutdown()
        server.server_close()
        thread.join()


def reply_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
               mobile: bool) -> str:
    """`#204 reply-arrow`: the arrows' geometry, and a real tap on one, against a fresh profile."""
    api = ReplyApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    platform = "Linux; Android 15; Pixel 7" if mobile else "X11; Linux x86_64"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                user_agent=(
                    f"Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko)"
                    f" Chrome/151.0.0.0 {'Mobile ' if mobile else ''}Safari/537.36"
                ),
                viewport={"width": width, "height": height},
                device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile,
                has_touch=mobile,
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-reply-{name}.png"))

            def where(row_id: str) -> dict[str, object]:
                found = page.evaluate(REPLY_ROW_JS, row_id)
                if not isinstance(found, dict):
                    raise AssertionError(f"{label}: row {row_id} could not be measured: {found!r}")
                return {str(key): value for key, value in found.items()}

            def number(found: dict[str, object], key: str) -> float:
                return float(str(found[key]))

            def tap(found: dict[str, object], key: str, why: str) -> None:
                """A real finger on a phone, a real mouse on a desk, at the centre `key` names."""
                at = found[key]
                if not isinstance(at, list) or len(at) != 2:
                    raise AssertionError(f"{label}: {why}: nothing to tap: {found}")
                x, y = float(at[0]), float(at[1])
                if mobile:
                    page.touchscreen.tap(x, y)
                else:
                    page.mouse.click(x, y)

            def wait_for(row_id: str, ready: str, why: str) -> dict[str, object]:
                deadline = time.monotonic() + 5
                found = where(row_id)
                while time.monotonic() < deadline and not page.evaluate(ready, found):
                    page.wait_for_timeout(50)
                    found = where(row_id)
                check(bool(page.evaluate(ready, found)), f"{label}: {why}: {found}")
                return found

            def arrows(state: str) -> None:
                if not mobile:
                    # Off every arrow, so no square being photographed has its hover disc behind it.
                    page.mouse.move(1, 1)
                listed = page.evaluate(REPLY_LIST_JS)
                problems = [str(problem) for problem in listed["problems"]]
                speakers: set[str] = set()
                states: set[str] = set()
                for row_id in listed["replies"]:
                    found = page.evaluate(REPLY_ARROW_JS, row_id)
                    problems += [str(problem) for problem in found["problems"]]
                    if "square" not in found:
                        continue
                    speakers.add(str(found["who"]))
                    states.add(str(found["state"]))
                    square = found["square"]
                    clip: FloatRect = {"x": float(square["x"]), "y": float(square["y"]),
                                       "width": float(square["width"]), "height": float(square["height"])}
                    png = page.screenshot(clip=clip)
                    drawn = page.evaluate(ARROW_INK_JS, {"png": base64.b64encode(png).decode("ascii"),
                                                         "square": square, "mark": found["mark"]})
                    which = f"{found['label']}, {found['state']}"
                    if float(drawn["contrast"]) < 3:
                        problems.append(f"{which}: the arrow is drawn {float(drawn['contrast']):.2f}:1 against"
                                        f" the page, as {drawn['ink']} on {drawn['ground']}")
                    accent = [round(float(value)) for value in re.findall(r"[0-9.]+", str(found["accent"]))[:3]]
                    if max(abs(int(seen) - want) for seen, want in zip(drawn["ink"], accent)) > 24:
                        problems.append(f"{which}: the arrow is drawn {drawn['ink']}, not the accent {accent}")
                check(not problems, f"{label}, {state}: {problems}")
                check(speakers == {"coder", "human", "me"},
                      f"{label}, {state}: the replies are from {sorted(speakers)}, not every kind of speaker")
                check(states == REPLY_STATES,
                      f"{label}, {state}: the replies are {sorted(states)}, not one in every state a row recedes in")

            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            page.click("#view-switch")
            page.wait_for_selector('#discord-log > li[data-id="215"]', timeout=10_000)
            arrows("at the default type size")

            # The reader at the newest line, the third party's answer on screen and the question
            # it answers more than a screen above it. A real tap on the arrow.
            page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; }")
            page.wait_for_timeout(200)
            reply = where("215")
            question = where("200")
            check(number(question, "bottom") < number(question, "line"),
                  f"{label}: the question was on screen before the tap: {question}")
            shot("1-before-tap")
            tap(reply, "arrow", "the reply's arrow")
            landed = wait_for("200", "(r) => r.landed === 'true'", "the tap did not land on the question")
            shot("2-landed")
            check(number(landed, "line") - 0.5 <= number(landed, "top") < number(landed, "foot") - 40,
                  f"{label}: the question landed at {landed['top']}px, not under the floating line at"
                  f" {landed['line']}px: {landed}")
            after = where("215")
            check(after["collapsed"] == reply["collapsed"],
                  f"{label}: the tap on the arrow also folded or opened the reply: {reply} -> {after}")
            check(after["chip"] is not None, f"{label}: the reply is off screen and Back to reply is not offered")

            # The way back, by a real tap on the chip.
            tap(after, "chip", "Back to reply")
            back = wait_for("215", "(r) => r.top < r.foot && r.bottom > r.line && r.chip === null",
                            "Back to reply did not bring the reply back and go")
            shot("3-back-to-reply")
            check(back["landed"] == "true", f"{label}: Back to reply did not say where it landed: {back}")

            # In reading mode a tap on a row reads it aloud. A tap on its arrow still only jumps.
            page.evaluate("() => document.getElementById('read-aloud').click()")
            page.evaluate("() => document.querySelector('#discord-log > li[data-id=\"215\"]')"
                          ".scrollIntoView({block: 'center'})")
            page.wait_for_timeout(300)
            with api.lock:
                seen = len(api.requests)
            tap(where("215"), "arrow", "the reply's arrow in reading mode")
            wait_for("200", "(r) => r.landed === 'true' && r.top < r.foot", "in reading mode the arrow did not jump")
            page.wait_for_timeout(300)
            with api.lock:
                spoken = [r for r in api.requests[seen:] if "/speak" in r or "/speech" in r]
            reading = where("215")
            check(not spoken and reading["reading"] != "true" and reading["pending"] != "true",
                  f"{label}: in reading mode the arrow's tap started reading the reply: {spoken} {reading}")
            page.evaluate("() => document.getElementById('read-aloud').click()")

            # A reader's large type: everything in the gutter is in rem, so it grows, and must still fit.
            large = page.add_style_tag(content="html { font-size: 150% !important; }")
            page.wait_for_timeout(200)
            arrows("at 150% type")
            shot("4-large-type")
            large.evaluate("element => element.remove()")

            # The owner's devices are in the dark scheme, where an accent close in value to the
            # surface is the defect this page has had before (see `data-reading` in web/voice.css).
            page.emulate_media(color_scheme="dark")
            page.wait_for_timeout(200)
            arrows("in the dark scheme")
            shot("5-dark")

            check(not errors, f"{label}: the page threw: {errors}")
            browser_version = context.browser.version if context.browser else "Chromium"
            context.close()
        return browser_version
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def tile_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
              mobile: bool) -> int:
    """`#213 row-side-borders`: the coding agent's tiles, with four sides, beside the other speakers and
    in every state that draws a border differently, in the dark theme the owner reads in. Answers how
    many rows were measured with every speaker on the screen."""
    api = TileApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-tiles-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile, has_touch=mobile, color_scheme="dark",
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-tiles-{name}.png"))

            def row(row_id: str) -> str:
                return f"document.querySelector('#discord-log > li[data-id=\"{row_id}\"]')"

            def wait_for(ready: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(ready):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(ready)), f"{label}: {why}")

            def measured(where: str) -> int:
                if not mobile:
                    page.mouse.move(1, 1)  # off every row, so none is drawn as hovered
                found = page.evaluate(TILE_JS, {"edge": mobile})
                check(not found["problems"], f"{label}, {where}: {found['problems']}")
                return int(found["count"])

            # Being read and asked to be read need audio, and the kept place a gesture; the page suite
            # and tests/read_aloud_browser.py drive those. What is measured here is how the stylesheet
            # draws each state on a tile, so the attribute the page would set is set here.
            def as_if(row_id: str, attribute: str) -> None:
                page.evaluate(f"() => {row(row_id)}.setAttribute('{attribute}', 'true')")

            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            page.click("#view-switch")
            page.wait_for_selector('#discord-log > li[data-id="211"]', timeout=10_000)
            page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; }")
            page.wait_for_selector("#summarise", state="visible", timeout=10_000)
            page.click("#summarise")
            wait_for(f"() => {row(SUMMARISED_ROW)}.getAttribute('data-summarised') === 'true'",
                     "the long row at the newest line was not summarised")
            wait_for(f"() => {row('209')}.getAttribute('data-replied') === 'true'"
                     f" && {row('206')}.getAttribute('data-own-read') === 'true'",
                     "the third party's row is not answered, or the owner's not read as his own")
            as_if(READING_ROW, "data-reading")
            # The owner's question just under the floating line, so every speaker, the reply, the
            # summary and the row being read are on the screen together.
            page.evaluate(f"""() => {{
                const area = document.getElementById('scroll-area');
                let line = area.getBoundingClientRect().top;
                for (const id of ['channel-freshness', 'search-toggle']) {{
                    const floating = document.getElementById(id);
                    if (floating && floating.getClientRects().length) line = Math.max(line, floating.getBoundingClientRect().bottom);
                }}
                area.scrollTop += {row('206')}.getBoundingClientRect().top - line - 8;
            }}""")
            page.wait_for_timeout(200)
            count = measured("with the owner's question at the head of the list")
            shot("1-speakers")

            # The top of the channel, where the rest of the states are; the failed summary is asked
            # for as it comes into view.
            page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
            wait_for(f"() => {row(FAILED_SUMMARY_ROW)}.getAttribute('data-summary-failed') === 'true'",
                     "the row whose summary fails was not drawn as failed")
            wait_for(f"() => {row('201')}.getAttribute('data-pinned') === 'true'"
                     f" && {row('202')}.getAttribute('data-archived') === 'true'"
                     f" && {row('203')}.getAttribute('data-noise') === 'true'",
                     "the pinned, archived and placeholder rows are not in their states")
            as_if(MARKED_ROW, "data-marked")
            as_if(PENDING_ROW, "data-pending")
            page.evaluate("() => { document.getElementById('scroll-area').scrollTop = 0; }")
            page.wait_for_timeout(200)
            measured("at the top")
            shot("2-states")

            if not mobile:
                # Under the pointer the whole border takes the accent, so all four sides do.
                page.evaluate("() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; }")
                page.wait_for_timeout(200)
                page.hover('#discord-log > li[data-id="211"] .body')
                found = page.evaluate(TILE_JS, {"edge": mobile})
                check(not found["problems"], f"{label}, under the pointer: {found['problems']}")
                shot("3-hover")

            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
        return count
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


class DockApi(FakeApi):
    """`#218 desktop-dock`: the ordinary channel, and one whose name is thirty characters, first."""

    def client_config(self, scope: str) -> Json:
        return {**super().client_config(scope), "channels": [LONG_CHANNEL, CHANNEL]}


def dock_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
              mobile: bool) -> str:
    """`#218 desktop-dock`: the dock on the call view and the channel, in the dark theme the owner
    reads in. On a desk: ordinary controls in one row where they fit and two where they do not, inside
    the column, overlapping nothing, the pickers wide enough for a thirty-character name, the switch
    in the same corner in every state and a second click in the same spot switching back, and every
    stop of a Tab walk ringed and unclipped. On a phone: the dock the phone had. Answers a summary."""
    api = DockApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    heights: dict[str, float] = {}
    # Where the view switch is drawn in each state measured in the default column.
    switches: dict[str, DockBox] = {}
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-dock-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile, has_touch=mobile, color_scheme="dark",
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            desk = not mobile

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-dock-{name}.png"), animations="disabled")

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{label}: {why}")

            def measured(state: str, order: list[str] | None, rows: int | None = None,
                         most: float = DOCK_DESK_MAX_PX,
                         whole: tuple[str, ...] = ("discord-channel", "thread-select")) -> DockFound:
                # Settled: a view that has just changed redraws its picker's label once its rows land,
                # so the dock is measured once two looks a tenth of a second apart agree.
                found: DockFound = page.evaluate(DOCK_JS, {"desk": desk, "order": order})
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    page.wait_for_timeout(100)
                    again: DockFound = page.evaluate(DOCK_JS, {"desk": desk, "order": order})
                    if again["boxes"] == found["boxes"]:
                        break
                    found = again
                where = f"{label}, {state}"
                check(not found["problems"], f"{where}: {found['problems']}")
                tall = float(found["height"])
                heights[state] = tall
                boxes = found["boxes"]
                if "view-switch" in boxes:
                    switches[state] = boxes["view-switch"]
                if desk:
                    check(tall <= most, f"{where}: the dock is {tall:.1f}px tall, more than {most}px")
                    if rows is not None:
                        check(found["rows"] == rows, f"{where}: the dock's controls make {found['rows']} rows, not {rows}")
                    if rows == 1:
                        check(tall <= DOCK_ONE_ROW_MAX_PX, f"{where}: one row stands {tall:.1f}px tall")
                    for picker in (found["pickers"][name] for name in whole if name in found["pickers"]):
                        check(picker["width"] >= picker["natural"] - 1,
                              f"{where}: the picker showing {picker['text']!r} is {picker['width']:.1f}px wide and"
                              f" needs {picker['natural']:.1f}px, so the name is cut short: {found['pickers']}")
                else:
                    check(abs(tall - PHONE_DOCK_PX) <= 1,
                          f"{where}: the phone's dock is {tall:.1f}px tall; it was {PHONE_DOCK_PX:.1f}px")
                    for big in ("hang-up", "talk", "read-aloud", "read-speed", "todo-filter"):
                        if big in boxes:
                            check(abs(boxes[big]["height"] - PHONE_SPAN_PX) <= 1,
                                  f"{where}: #{big} is {boxes[big]['height']:.1f}px tall, not the tile's two rows")
                    for narrow in ("todo-filter", "speaker", "clear-view"):
                        if narrow in boxes:
                            check(abs(boxes[narrow]["width"] - PHONE_NARROW_PX) <= 1,
                                  f"{where}: #{narrow} is {boxes[narrow]['width']:.1f}px wide, not the tile's narrow column")
                return found

            def tabbed(state: str, found: DockFound) -> None:
                """Tab from the gear to the last control, every stop ringed: the bar's controls in the
                order the eye reads them, then the switch, then the pane's in the order the eye reads
                them. The switch is drawn in the corner, after the pane, so that it never moves; Tab
                reaches it where the markup has it — the end of the bar — as it does on a phone and
                in the bar's other home, in the header."""
                bar = [stop for stop in found["visual"] if stop not in found["pane"] and stop != "view-switch"]
                expected = bar + (["view-switch"] if "view-switch" in found["visual"] else []) + found["pane"]
                check(sorted(expected) == sorted(found["visual"]),
                      f"{label}, {state}: the dock shows {found['visual']}, which is not the bar, the switch"
                      f" and the pane {found['pane']}")
                page.focus("#open-settings")
                page.keyboard.press("Tab")
                page.keyboard.press("Shift+Tab")
                stops: list[str] = []
                for _ in range(len(found["visual"]) + 2):
                    stop = page.evaluate(DOCK_FOCUS_JS)
                    if stop["id"] is None:
                        break
                    check(stop["visible"], f"{label}, {state}: #{stop['id']} took focus without showing it")
                    check(not stop["problems"], f"{label}, {state}: {stop['problems']}")
                    stops.append(str(stop["id"]))
                    page.keyboard.press("Tab")
                check(stops == expected, f"{label}, {state}: Tab goes {stops}, not {expected}")

            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            until("() => !document.getElementById('control-pane').hidden", "the call view's pane did not appear")

            # The call view, idle: the bar's four and the switch, and Sound, Clear and Talk beside them.
            idle = measured("call view, idle", ["speaker", "clear-view", "talk"], rows=1 if desk else None)
            shot("1-call-idle")
            # A transcript long enough to scroll, and the reader on its newest line as they press Talk.
            check(bool(page.evaluate(DOCK_TRANSCRIPT_JS)), f"{label}: the transcript does not scroll")
            # A live call, as renderControls draws it: Hang up shown and Talk listening. The audio is
            # tests/read_aloud_browser.py's and the screenshot harness's; the layout is this one's.
            page.evaluate("""() => { document.getElementById('hang-up').hidden = false;
                document.getElementById('control-pane').className = '';
                document.getElementById('talk').className = 'control control-talk live';
                document.getElementById('talk-label').textContent = 'Listening'; }""")
            live = measured("call view, live", ["speaker", "clear-view", "hang-up", "talk"])
            # On a desk that was a second row, taken from the bottom of the list: the reader who was on
            # the newest turn is on it still, close enough that the call's next turn is followed.
            place = page.evaluate(DOCK_PLACE_JS)
            check(float(place["gap"]) <= BOTTOM_SLACK_PX,
                  f"{label}: the dock grew from {idle['height']:.0f}px to {live['height']:.0f}px and left the"
                  f" reader {place['gap']:.0f}px above the newest turn, past the {BOTTOM_SLACK_PX}px it follows from")
            shot("2-call-live")
            if desk:
                tabbed("call view, live", live)
            # Up the history now, for the dock growing again below a reader who is not following.
            up = page.evaluate("""() => { const area = document.getElementById('scroll-area');
                area.scrollTop = Math.round((area.scrollHeight - area.clientHeight) / 2); return area.scrollTop; }""")
            # After it, the one-clause note under Start a new call: it goes under the word.
            page.evaluate("""() => { document.getElementById('hang-up').hidden = true;
                document.getElementById('control-pane').className = 'solo';
                document.getElementById('talk').className = 'control control-talk';
                document.getElementById('talk-label').textContent = 'Start a new call';
                const note = document.getElementById('talk-note');
                note.textContent = 'the agent starts fresh — the earlier conversation was too long to replay';
                note.hidden = false; }""")
            ended = measured("call view, after a call", ["speaker", "clear-view", "talk"], most=DOCK_NOTE_MAX_PX)
            check(abs(float(page.evaluate(DOCK_PLACE_JS)["top"]) - float(up)) <= 0.5,
                  f"{label}: the dock grew from {live['height']:.0f}px to {ended['height']:.0f}px and moved a reader"
                  " who was up the history")
            shot("3-call-ended")

            # The channel, whose thirty-character name opens first, in All. On a desk it is opened by a
            # real click in the middle of where the switch stood on the idle call view, and later
            # closed by another in the same spot: the switch is the control the reader flicks, and
            # after a flick it has to be under the pointer that flicked it.
            page.reload(wait_until="load")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            until("() => !document.getElementById('control-pane').hidden", "the call view's pane did not appear")
            spot = switches["call view, idle"]
            spot_x, spot_y = spot["left"] + spot["width"] / 2, spot["top"] + spot["height"] / 2

            def flick(checked: str, why: str) -> None:
                if desk:
                    page.mouse.click(spot_x, spot_y)
                else:
                    page.click("#view-switch")
                until("() => document.getElementById('view-switch').getAttribute('aria-checked') === "
                      f"{json.dumps(checked)}", why)

            flick("true", "a click where the switch stood on the call view did not open the channel")
            page.wait_for_selector("#discord-log > li[data-id]", timeout=10_000)
            check(page.evaluate("() => document.getElementById('discord-channel').selectedOptions[0].textContent")
                  == LONG_CHANNEL["label"] and len(str(LONG_CHANNEL["label"])) >= 30,
                  f"{label}: the channel open is not the thirty-character one")
            reading = ["todo-filter", "read-speed", "read-aloud"]
            channel = measured("the channel, a long name", reading)
            shot("4-channel-long-name")
            if desk:
                tabbed("the channel, a long name", channel)
                # Two rows here: the pickers on the first, and on the second the reading buttons at its
                # right, beside the switch.
                boxes = channel["boxes"]
                switch, read, picker = boxes["view-switch"], boxes["read-aloud"], boxes["discord-channel"]
                middle = {name: box["top"] + box["height"] / 2 for name, box in boxes.items()}
                check(channel["rows"] == 2 and abs(middle["view-switch"] - middle["read-aloud"]) < 4
                      and middle["discord-channel"] < middle["read-aloud"] - 4
                      and read["left"] + read["width"] < switch["left"],
                      f"{label}: the reading buttons are not beside the switch on a second row, under the"
                      f" pickers: {picker}, {read}, {switch}")
                # Pace's popover, opened by a real click: over the list, above the dock and inside the
                # column. Placed against the pane, it opened over the row above the pane — the switch
                # and the pickers, under it until it was dismissed.
                pace = boxes["read-speed"]
                pace_x, pace_y = pace["left"] + pace["width"] / 2, pace["top"] + pace["height"] / 2
                page.mouse.click(pace_x, pace_y)
                until("() => !document.getElementById('speed-popover').hidden", "Pace did not open its popover")
                shot("4b-pace-open")
                problems = page.evaluate(DOCK_POPOVER_JS)
                check(not problems, f"{label}: the pace popover, open: {problems}")
                page.mouse.click(pace_x, pace_y)
                until("() => document.getElementById('speed-popover').hidden", "Pace did not close its popover")
                # The same spot again is the switch again: back to the call, and back to the channel.
                flick("false", "a second click in the same spot did not switch back to the call")
                until("() => !document.getElementById('talk').hidden", "the call view's Talk did not come back")
                flick("true", "a third click in the same spot did not open the channel again")
                until("() => document.getElementById('read-aloud').hidden === false",
                      "the channel's Read did not come back")
            # A thread open in the view picker beside the long name. In the default column the two
            # names, the gear, the device button and the switch do not fit one row, and the VIEW
            # picker gives way: the channel's name stays whole.
            thread_option = page.evaluate(
                "() => [...document.getElementById('thread-select').options].map((o) => o.value)"
                ".find((v) => v.startsWith('thread:'))")
            check(bool(thread_option), f"{label}: the view picker offers no thread")
            page.select_option("#thread-select", str(thread_option))
            until("() => document.getElementById('thread-select').value.startsWith('thread:')",
                  "the thread did not open")
            measured("a thread beside the long name", reading, whole=("discord-channel",))
            shot("5-channel-thread")
            page.select_option("#thread-select", "flat")
            # A short name: everything shares one row on a desk.
            page.select_option("#discord-channel", str(CHANNEL["id"]))
            until("() => document.getElementById('discord-channel').value === "
                  f"{json.dumps(CHANNEL['id'])}", "the short channel did not open")
            measured("the channel, a short name", reading, rows=1 if desk else None)
            shot("6-channel-short-name")

            if desk:
                # ONE PLACE FOR THE SWITCH in every state of the default column: idle, live, after a
                # call, a long name over two rows, a thread, a short name in one. Its right edge and its
                # bottom are the same in all of them, and so is its width, whichever word it shows.
                spread = {edge: max(values) - min(values) for edge, values in (
                    ("right", [b["left"] + b["width"] for b in switches.values()]),
                    ("bottom", [b["top"] + b["height"] for b in switches.values()]),
                    ("width", [b["width"] for b in switches.values()]))}
                check(len(switches) >= 6 and all(moved <= 0.5 for moved in spread.values()),
                      f"{label}: the switch moves between states ({spread}): {switches}")

                # The column dragged wide with the reader's own control: the long name and the three
                # reading buttons share one row.
                page.select_option("#discord-channel", str(LONG_CHANNEL["id"]))
                until("() => document.getElementById('discord-channel').value === "
                      f"{json.dumps(LONG_CHANNEL['id'])}", "the long channel did not open again")
                page.evaluate("""() => { const range = document.getElementById('reading-width');
                    range.value = '110'; range.dispatchEvent(new Event('input')); }""")
                until("() => document.documentElement.style.getPropertyValue('--reading-width') === '110ch'",
                      "the width handle's control did not take")
                measured("a wide column, the long name", reading, rows=1)
                shot("7-wide-column")
                # And pulled in to its narrowest, where the bar's own row truly is out of room: the
                # view picker at its floor, the channel's name the one cut, ending in an ellipsis, and
                # nothing pushed out of the pack's sight.
                page.evaluate("""() => { const range = document.getElementById('reading-width');
                    range.value = range.min; range.dispatchEvent(new Event('input')); }""")
                until("() => document.documentElement.style.getPropertyValue('--reading-width') === "
                      "document.getElementById('reading-width').min + 'ch'", "the width handle's control did not narrow")
                narrow = measured("the narrowest column, the long name", reading, whole=("thread-select",))
                cut = narrow["pickers"]["discord-channel"]
                floor = narrow["boxes"]["thread-select"]["width"]
                check(cut["width"] < cut["natural"] and cut["width"] >= 5 * 16 - 0.5 and floor >= 5 * 16 - 0.5,
                      f"{label}: in the narrowest column the pickers are {cut['width']:.1f}px and {floor:.1f}px")
                check(page.evaluate("() => getComputedStyle(document.getElementById('discord-channel')).textOverflow")
                      == "ellipsis", f"{label}: a cut channel name does not say it was cut")
                shot("8-narrow-column")

            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
        summary = ", ".join(f"{state} {tall:.0f}px" for state, tall in heights.items())
        return summary if desk else f"{summary} (the phone's dock as it was, {PHONE_DOCK_PX:.0f}px)"
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def coalesce_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
                  mobile: bool) -> None:
    """`#214 reply-arrow` and `#215 reply-coalesce`: both arrows as drawn, and replies gathered under a
    message and put back by real taps, against a fresh profile in the dark scheme."""
    api = CoalesceApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    platform = "Linux; Android 15; Pixel 7" if mobile else "X11; Linux x86_64"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-") as profile:
            context = chromium.launch_persistent_context(
                profile,
                headless=True,
                executable_path=args.browser_executable,
                user_agent=(
                    f"Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko)"
                    f" Chrome/151.0.0.0 {'Mobile ' if mobile else ''}Safari/537.36"
                ),
                viewport={"width": width, "height": height},
                device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile,
                has_touch=mobile,
                color_scheme="dark",
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-coalesce-{name}.png"))

            def rows() -> list[dict[str, object]]:
                return [{str(key): value for key, value in row.items()} for row in page.evaluate(COALESCE_ROWS_JS)]

            def order() -> list[str]:
                return [str(row["id"]) for row in rows()]

            def top(row_id: str) -> float:
                return float(str(next(row["top"] for row in rows() if row["id"] == row_id)))

            def look(kind: str, row_id: str, parent: str = "", stack: list[str] | None = None,
                     why: str = "") -> dict[str, object]:
                page.evaluate(CENTRE_ROW_JS, row_id)
                page.wait_for_timeout(120)
                # `#228 reply-visible`. A head is found by the accent in its pixels, and a circle on
                # the row above's corner is the accent too: photographed without it. That the two
                # keep apart is `circle_beside`'s to say, with the circle drawn.
                hidden = page.add_style_tag(content="#discord-log .reply-marker { visibility: hidden !important; }") \
                    if kind in ("below", "side") else None
                png = base64.b64encode(page.screenshot()).decode("ascii")
                if hidden is not None:
                    hidden.evaluate("element => element.remove()")
                found = page.evaluate(COALESCE_LOOK_JS, {"png": png, "check": kind, "id": row_id, "parent": parent,
                                                         "stack": stack or []})
                problems = [str(problem) for problem in found["problems"]]
                check(not problems, f"{label}, {why}: {problems}")
                return {str(key): value for key, value in found.items()}

            def head(kind: str, row_id: str, parent: str, over: bool, why: str) -> tuple[float, float]:
                """Where the adjacent arrow of `row_id` has its head, and that a press there is that arrow."""
                found = look(kind, row_id, parent, why=why)
                hit = page.evaluate(HEAD_HIT_JS, {"id": row_id, "kind": kind, "tip": found["tip"], "over": over})
                problems = [str(problem) for problem in hit["problems"]]
                check(not problems, f"{label}, {why}: {problems}")
                circle_beside(kind, row_id, parent, found["tip"], why)
                return float(hit["at"][0]), float(hit["at"][1])

            def circle_beside(kind: str, row_id: str, parent: str, tip: object, why: str) -> None:
                """`#228 reply-visible`. A circle on the row above's corner keeps clear of the head."""
                found = page.evaluate(CIRCLE_BESIDE_HEAD_JS, {"id": row_id, "parent": parent, "kind": kind,
                                                              "tip": tip})
                if found["circle"]:
                    circles_beside.append(f"{row_id} under {parent} ({kind}{', lifted' if found['lifted'] else ''})")
                problems = [str(problem) for problem in found["problems"]]
                check(not problems, f"{label}, {why}: {problems}")

            def press(x: float, y: float, twice: bool = False) -> None:
                if mobile:
                    page.touchscreen.tap(x, y)
                    if twice:
                        page.touchscreen.tap(x, y)
                elif twice:
                    page.mouse.dblclick(x, y)
                else:
                    page.mouse.click(x, y)

            def tap(row_id: str, part: str, why: str, twice: bool = False) -> None:
                at = page.evaluate(TARGET_JS, {"id": row_id, "part": part})
                if not isinstance(at, list):
                    raise AssertionError(f"{label}: {why}: nothing to tap on {row_id}")
                press(float(at[0]), float(at[1]), twice)

            def wait_until(ready: str, why: str) -> None:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline and not page.evaluate(ready):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(ready)), f"{label}: {why}")

            circles_beside: list[str] = []

            def arrows(why: str, default_size: bool) -> None:
                styles = {str(row["id"]): (row["style"], row["reach"]) for row in rows() if row["style"]}
                wanted = {"207": ("adjacent", "below"), "208": ("adjacent", "side"),
                          "211": ("adjacent", "below" if mobile else "side"), "213": ("disconnected", None),
                          "215": ("disconnected", None), "216": ("adjacent", "side")}
                for row_id, style in wanted.items():
                    check(styles.get(row_id) == style,
                                  f"{label}, {why}: {row_id}'s arrow is {styles.get(row_id)}, not {style}")
                # A head over the row above's arrow is the default size's case: larger type makes the
                # short row above taller than the square at its middle reaches.
                for kind, row_id, parent, whose, over in adjacent_heads(mobile):
                    head(kind, row_id, parent, over and default_size, f"{why}, {whose}")
                look("disconnected", "213", why=f"{why}, a reply to a message far above")

            def bridge(why: str) -> None:
                for row_id in GATHERED_STACK:
                    look("bridge", row_id, "206", GATHERED_STACK, why=f"{why}, at {row_id}")

            page.goto(url, wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            page.click("#view-switch")
            page.wait_for_selector('#discord-log > li[data-id="214"]', timeout=10_000)
            if not mobile:
                # Off every control, so nothing photographed has its hover disc behind it.
                page.mouse.move(1, 1)
            page.wait_for_timeout(200)
            check(order() == COALESCE_SCATTERED, f"{label}: the channel is not what this walk assumes: {order()}")

            # Both arrows, at the ordinary type size and at 150%: everything in the gutter is in rem,
            # and where the head meets the row above is measured again when the type grows.
            arrows("at the default type size", True)
            # `#228 reply-visible`. 206 and 210 are each answered from directly below and from further
            # away, so each has a circle beside the head that meets it. On a phone 211's head comes up
            # under the owner's inset row too near its corner for both, and the circle is lifted; on a
            # desk it meets the row's side, raised, and the circle stays on the corner.
            wanted_211 = "211 under 210 (below, lifted)" if mobile else "211 under 210 (side)"
            check(wanted_211 in circles_beside and any(seen.startswith("207 under 206") for seen in circles_beside),
                  f"{label}: the circles beside a head were not the ones this walk assumes: {circles_beside}")
            page.evaluate(CENTRE_ROW_JS, "208")
            page.wait_for_timeout(120)
            shot("1-adjacent")
            page.evaluate(CENTRE_ROW_JS, "213")
            page.wait_for_timeout(120)
            shot("2-disconnected")
            large = page.add_style_tag(content="html { font-size: 150% !important; }")
            page.wait_for_timeout(250)
            arrows("at 150% type", False)
            large.evaluate("element => element.remove()")
            page.wait_for_timeout(250)

            # A real press on each adjacent arrow's HEAD, the part a reader aims at: it jumps to the row
            # above and lights it. Then two on 216's, whose head is drawn over 215's own arrow: both are
            # 216's arrow's, and they gather the conversation 216 is in under its head, 210, which 215
            # answers.
            pressing = "tap" if mobile else "click"
            for kind, row_id, parent, whose, over in adjacent_heads(mobile):
                x, y = head(kind, row_id, parent, over, f"before a {pressing} on its head, {whose}")
                page.evaluate(LANDING_LATCH_JS, parent)
                press(x, y)
                wait_until(f"() => window.__landedOn === {json.dumps(parent)}",
                           f"a {pressing} on {row_id}'s head did not jump to {parent}; the status line says"
                           f" {page.evaluate('() => document.getElementById(\"status\").textContent')!r}")
            x, y = head("side", "216", "215", True, f"before two {pressing}s on its head")
            page.evaluate(ARROW_PRESS_LATCH_JS)
            press(x, y, twice=True)
            wait_until("() => document.querySelector('#discord-log > li[data-coalesce=\"parent\"]') !== null",
                       f"two {pressing}s on 216's head gathered nothing")
            pressed_arrows = page.evaluate("() => window.__arrowPresses")
            check(pressed_arrows == ["216", "216"], f"{label}: two {pressing}s on 216's head pressed the arrows of {pressed_arrows}")
            gathered_under = page.evaluate("() => document.querySelector('#discord-log > li[data-coalesce=\"parent\"]')"
                                           ".getAttribute('data-id')")
            check(gathered_under == "210", f"{label}: two {pressing}s on 216's head gathered under {gathered_under}")
            under_210 = [*COALESCE_SCATTERED[:11], "211", "215", "216", *COALESCE_SCATTERED[12:15],
                         *COALESCE_SCATTERED[17:]]
            check(order() == under_210, f"{label}: two {pressing}s on 216's head gathered its conversation as {order()}")
            # Past the moment after two presses in which the X that has just appeared ignores a third.
            page.wait_for_timeout(400)
            page.evaluate(CENTRE_ROW_JS, "211")
            tap("211", "x", "the X beside the bridge from 210")
            wait_until("() => document.querySelector('#discord-log > li[data-coalesce]') === null",
                       "the X did not put 210's conversation back")
            check(order() == COALESCE_SCATTERED, f"{label}: 210's conversation went back as {order()}")

            # A real tap on the root's N replies: its replies gathered under it, the root unmoved.
            page.evaluate(CENTRE_ROW_JS, "206")
            page.evaluate("() => { const a = document.getElementById('scroll-area');"
                          " a.scrollTop += document.querySelector('#discord-log > li[data-id=\"206\"]')"
                          ".getBoundingClientRect().top - a.getBoundingClientRect().top - 90; }")
            page.wait_for_timeout(150)
            before = top("206")
            tap("206", "chip", "the root's N replies")
            wait_until("() => document.querySelector('#discord-log > li[data-coalesce=\"parent\"]') !== null",
                       "the N replies chip did not gather the replies")
            page.wait_for_timeout(150)
            check(abs(top("206") - before) <= 1,
                          f"{label}: gathering moved the root from {before}px to {top('206')}px")
            check(order() == COALESCE_GATHERED, f"{label}: the replies were gathered as {order()}")
            shot("3-gathered")
            bridge("gathered")

            # Opening a folded reply in the stack, by a real tap on its text, and larger type: the
            # bridge stays attached through both.
            folded = page.evaluate("() => document.querySelector('#discord-log > li[data-id=\"212\"]')"
                                   ".getAttribute('data-collapsed')")
            page.evaluate(CENTRE_ROW_JS, "212")
            tap("212", "body", "a gathered reply's text")
            page.wait_for_timeout(200)
            check(page.evaluate("() => document.querySelector('#discord-log > li[data-id=\"212\"]')"
                                        ".getAttribute('data-collapsed')") != folded,
                          f"{label}: a tap on a gathered reply's text did not open it")
            bridge("with a gathered reply opened")
            large = page.add_style_tag(content="html { font-size: 150% !important; }")
            page.wait_for_timeout(250)
            bridge("at 150% type")
            large.evaluate("element => element.remove()")
            page.wait_for_timeout(250)

            # The X, by a real tap: back in time order, the root unmoved.
            page.evaluate(CENTRE_ROW_JS, GATHERED_STACK[0])
            page.wait_for_timeout(120)
            before = top("206")
            tap(GATHERED_STACK[0], "x", "the X beside the bridge")
            wait_until("() => document.querySelector('#discord-log > li[data-coalesce]') === null",
                       "the X did not put the replies back")
            page.wait_for_timeout(150)
            check(order() == COALESCE_SCATTERED, f"{label}: the X put the replies back as {order()}")
            check(abs(top("206") - before) <= 1,
                          f"{label}: putting the replies back moved the root from {before}px to {top('206')}px")

            # Two real taps on the arrow of a reply far below the root: gathered, the reply unmoved.
            page.evaluate(CENTRE_ROW_JS, "213")
            page.wait_for_timeout(150)
            before = top("213")
            tap("213", "arrow", "the arrow of a reply far below its root", twice=True)
            wait_until("() => document.querySelector('#discord-log > li[data-coalesce=\"parent\"]') !== null",
                       "two taps on the arrow did not gather the replies")
            page.wait_for_timeout(400)
            check(order() == COALESCE_GATHERED, f"{label}: two taps gathered the replies as {order()}")
            after = top("213")
            viewport = float(page.evaluate("() => document.getElementById('scroll-area').getBoundingClientRect().bottom"))
            check(abs(after - before) <= 1 and 0 <= after < viewport,
                          f"{label}: the tapped reply moved from {before}px to {after}px")
            check(page.evaluate("() => !document.querySelector('#discord-log > li[data-landed=\"true\"]')"),
                          f"{label}: the first of two taps jumped away")
            shot("4-double-tap")

            check(not errors, f"{label}: the page threw: {errors}")
            context.close()
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def pinned_elsewhere(api: FakeApi) -> FakeApi:
    """`api` with the thread root, row 201, already pinned — by another device, before this page."""
    api.serve_pins_revision = True
    api.pins["201"] = {
        "message_id": "201", "author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True,
        "content": "a thread root", "truncated": False, "timestamp": str(api.messages[1]["timestamp"]),
        "thread_id": "spaces/A/threads/one", "thread_root": True, "pinned_at_ms": 1_790_000_000_000,
    }
    api.pins_revision = 1
    return api


def menu_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
              root_percent: int) -> None:
    """`#206 pin-message`: every row's ⋯ menu on the screen where the meta line wraps, in both shapes.

    A phone in the dark theme, at one of `MENU_SCALES`. Each row's menu in the channel is opened by a
    real tap and measured, then the pinned row's in the Pinned filter, whose Unpin is then tapped for
    real — the item a menu hung off the left of the screen put out of reach.
    """
    for provider, upstream in (("Discord", False), ("Google Chat", True)):
        api = pinned_elsewhere(FakeApi())
        api.provider_name, api.upstream_read = provider, upstream
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        shape = f"{label}, {provider}"
        try:
            with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-menus-") as profile:
                context = chromium.launch_persistent_context(
                    profile, headless=True, executable_path=args.browser_executable,
                    viewport={"width": width, "height": height}, device_scale_factor=2.625,
                    is_mobile=True, has_touch=True, color_scheme="dark",
                )
                page = context.pages[0]
                larger_root_font(page, root_percent)
                errors: list[str] = []
                page.on("pageerror", lambda error: errors.append(str(error)))

                def until(script: str, why: str) -> None:
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline and not page.evaluate(script):
                        page.wait_for_timeout(50)
                    check(bool(page.evaluate(script)), f"{shape}: {why}")

                def tap(selector: str) -> None:
                    # Scrolled to the middle of the list first: at a large font a row's controls can
                    # be past either end of it, and the floating bar sits over its top.
                    found = page.evaluate(
                        "(selector) => { const e = document.querySelector(selector);"
                        " const area = document.getElementById('scroll-area');"
                        " if (area.contains(e)) { const a = area.getBoundingClientRect(), b = e.getBoundingClientRect();"
                        "   area.scrollTop += (b.top + b.bottom) / 2 - (a.top + a.bottom) / 2; }"
                        " const b = e.getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }",
                        selector)
                    page.touchscreen.tap(float(found[0]), float(found[1]))

                def menu_of(list_id: str, row_id: str) -> str:
                    row = f'#{list_id} > li[data-id="{row_id}"]'
                    hidden = f"document.querySelector('{row} .row-more-menu').hidden"
                    tap(f"{row} .row-more-button")
                    until(f"() => !{hidden}", f"row {row_id}'s ⋯ menu in #{list_id} did not open")
                    menu = page.evaluate(MENU_JS, {"list": list_id, "id": row_id, "upstream": upstream})
                    if args.screenshots:
                        page.screenshot(path=str(args.screenshots / f"{label}-{provider.split()[0].lower()}-"
                                                                     f"menu-{list_id}-{row_id}.png"))
                    check(not menu["problems"], f"{shape}, row {row_id}'s ⋯ menu in #{list_id}: {menu['problems']}"
                                                f" ({menu['items']})")
                    return hidden

                page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
                page.fill("#api-token", TOKEN)
                page.click("#save-token")
                page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
                page.click("#view-switch")
                until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 3",
                      "the channel did not open")
                until("() => { const c = document.querySelector('#discord-log > li[data-id=\"201\"] .pin-chip');"
                      " return Boolean(c) && !c.hidden; }", "the pin made elsewhere never showed on its row")
                for row_id in ("200", "201", "202"):
                    hidden = menu_of("discord-log", row_id)
                    page.keyboard.press("Escape")
                    until(f"() => {hidden}", f"Escape left row {row_id}'s ⋯ menu open")

                # The pinned row again, in the Pinned filter, and its Unpin reached by a thumb.
                tap("#search-toggle")
                until("() => !document.getElementById('pinned-filter').hidden", "the open bar has no Pinned filter")
                tap("#pinned-filter")
                until("() => document.querySelectorAll('#pinned-log > li[data-id]').length === 1",
                      "the pinned row did not show in the filter")
                menu_of("pinned-log", "201")
                tap('#pinned-log > li[data-id="201"] .row-pin-button')
                until("() => !document.querySelector('#pinned-log > li[data-id]')", "Unpin left the row in the filter")
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and "201" in api.pins:
                    page.wait_for_timeout(50)
                check("201" not in api.pins, f"{shape}: a tap on Unpin never reached the server")
                check(not errors, f"{shape}: the page threw: {errors}")
                context.close()
        finally:
            api.stopping.set()
            server.shutdown()
            server.server_close()
            thread.join()


def pin_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
             mobile: bool) -> None:
    """`#206 pin-message`, in the dark theme the owner reads in, against a fresh fake API."""
    api = pinned_elsewhere(FakeApi())
    # The shape whose ⋯ menu is widest: the read group holds both items, the longer label naming the
    # service. `menu_walk` takes the one-item shape too.
    api.provider_name, api.upstream_read = "Google Chat", True
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-pins-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile, has_touch=mobile, color_scheme="dark",
            )
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-pins-{name}.png"))

            def tap(selector: str) -> None:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                if mobile:
                    page.touchscreen.tap(float(found[0]), float(found[1]))
                else:
                    page.mouse.click(float(found[0]), float(found[1]))

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{label}: {why}")

            def ids(list_id: str) -> list[str]:
                return [str(row) for row in page.eval_on_selector_all(
                    f"#{list_id} > li[data-id]", "items => items.map(i => i.getAttribute('data-id'))")]

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 3", "the channel did not open")

            # A pin made elsewhere: read because the channel's read carried its revision.
            until("() => { const c = document.querySelector('#discord-log > li[data-id=\"201\"] .pin-chip');"
                  " return Boolean(c) && !c.hidden; }", "the pin made elsewhere never showed on its row")
            chip = page.evaluate(CHIP_JS, "201")
            shot("1-pinned-chip")
            check(not chip["problems"], f"{label}, the pinned chip in the dark theme: {chip['problems']}")

            # The ⋯ menu, opened by a real tap: on the pinned row, whose chip takes room on its meta
            # line, and closed by Escape; then on an unpinned one, left open for its Pin.
            for row_id in ("201", "200"):
                hidden = f"document.querySelector('#discord-log > li[data-id=\"{row_id}\"] .row-more-menu').hidden"
                tap(f'#discord-log > li[data-id="{row_id}"] .row-more-button')
                until(f"() => !{hidden}", f"row {row_id}'s ⋯ menu did not open")
                menu = page.evaluate(MENU_JS, {"list": "discord-log", "id": row_id, "upstream": True})
                shot(f"2-menu-{row_id}")
                check(not menu["problems"], f"{label}, row {row_id}'s ⋯ menu: {menu['problems']} ({menu['items']})")
                if row_id == "201":
                    page.keyboard.press("Escape")
                    until(f"() => {hidden}", "Escape left the ⋯ menu open")

            # Pin, shown at once and stored.
            tap('#discord-log > li[data-id="200"] .row-pin-button')
            until("() => { const c = document.querySelector('#discord-log > li[data-id=\"200\"] .pin-chip');"
                  " return Boolean(c) && !c.hidden; }", "Pin did not show on the row")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and "200" not in api.pins:
                page.wait_for_timeout(50)
            check("200" in api.pins, f"{label}: the pin never reached the server")

            # The Pinned filter on the open bar, before and after the count takes its room.
            tap("#search-toggle")
            until("() => !document.getElementById('pinned-filter').hidden", "the open bar has no Pinned filter")
            bare = page.evaluate(FILTER_BAR_JS, {"pinned": True, "fieldMin": 140})
            check(not bare["problems"], f"{label}, the filters on the open bar: {bare['problems']}")
            page.fill("#search-field", "a")
            counted = page.evaluate(FILTER_BAR_JS, {"pinned": True, "fieldMin": 140})
            shot("3-filter-beside-glass")
            check(not counted["problems"], f"{label}, the filters beside a count: {counted['problems']}")
            page.fill("#search-field", "")

            # Its tap shows the pins, and none of them is under the bar.
            tap("#pinned-filter")
            until("() => document.getElementById('pinned-filter').getAttribute('aria-pressed') === 'true'",
                  "a tap on the filter did not turn it on")
            until("() => document.querySelectorAll('#pinned-log > li[data-id]').length === 2", "the pins did not show")
            check(ids("pinned-log") == ["200", "201"], f"{label}: the filter showed {ids('pinned-log')}")
            covered = page.evaluate("() => { const bar = document.getElementById('search-float').getBoundingClientRect();"
                                    " document.getElementById('scroll-area').scrollTop = 0;"
                                    " const first = document.querySelector('#pinned-log > li');"
                                    " return first.getBoundingClientRect().top - bar.bottom; }")
            shot("4-pinned-shown")
            check(float(covered) >= -0.5, f"{label}: the open bar covers {-float(covered):.1f}px of the first pin")
            check(page.evaluate("() => document.getElementById('discord-log').hidden"),
                  f"{label}: the channel's list stayed up under the filter")

            # The glass folds the bar, and the filter with it.
            tap("#search-toggle")
            until("() => document.getElementById('pinned-log').hidden", "folding the bar left the filter on")
            check(page.evaluate("() => document.getElementById('pinned-filter').getAttribute('aria-pressed')") == "false",
                  f"{label}: the filter still says it is on")
            check(ids("discord-log") == ["200", "201", "202"], f"{label}: the channel came back as {ids('discord-log')}")
            check(not errors, f"{label}, pins: the page threw: {errors}")
            context.close()
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def links_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
               mobile: bool) -> None:
    """`#211 link-filter`, in the dark theme the owner reads in, against a fresh fake API.

    The glass larger than it was; the open bar holding Links and Pinned beside the glass, each a 44px
    target clear of the field and the glass, with the field still 140px wide; Links showing only the
    rows with links, each link real and on its own line; a real tap on a link opening a new tab
    without folding its row; and, over the call view, Links beside the glass on its own.
    """
    api = LinkApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-links-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile, has_touch=mobile, color_scheme="dark",
            )
            opened: list[str] = []

            def answer(route: Route) -> None:
                opened.append(route.request.url)
                route.fulfill(status=200, content_type="text/html", body="<title>opened</title>opened")

            for pattern in LINK_HOSTS:
                context.route(pattern, answer)
            page = context.pages[0]
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-links-{name}.png"))

            def point(selector: str) -> tuple[float, float]:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                return float(found[0]), float(found[1])

            def tap(selector: str) -> None:
                x, y = point(selector)
                if mobile:
                    page.touchscreen.tap(x, y)
                else:
                    page.mouse.click(x, y)

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{label}: {why}")

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 5", "the channel did not open")
            glass = page.evaluate("() => { const b = document.getElementById('search-toggle').getBoundingClientRect();"
                                  " return [b.width, b.height]; }")
            check(float(glass[0]) >= GLASS_MIN_PX and float(glass[1]) >= GLASS_MIN_PX,
                  f"{label}: the glass is drawn {glass[0]}x{glass[1]}px, under {GLASS_MIN_PX}px")
            shot("1-glass")

            # The open bar: Links, then Pinned, then the glass; then the same with a count beside them.
            tap("#search-toggle")
            until("() => !document.getElementById('links-filter').hidden", "the open bar has no Links filter")
            bare = page.evaluate(FILTER_BAR_JS, {"pinned": True, "fieldMin": 140})
            check(not bare["problems"], f"{label}, the filters on the open bar: {bare['problems']}")
            page.fill("#search-field", "a")
            counted = page.evaluate(FILTER_BAR_JS, {"pinned": True, "fieldMin": 140})
            shot("2-bar-with-count")
            check(not counted["problems"], f"{label}, the filters beside a count: {counted['problems']}")
            page.fill("#search-field", "")

            # A real tap on Links: only the rows with links, each showing only its links.
            folded = page.evaluate("() => document.querySelector('#discord-log > li[data-id=\"200\"]')"
                                   ".getAttribute('data-collapsed')")
            check(folded == "true", f"{label}: the long message did not arrive folded ({folded!r})")
            tap("#links-filter")
            until("() => document.getElementById('links-filter').getAttribute('aria-pressed') === 'true'",
                  "a tap on Links did not turn it on")
            until("() => document.querySelectorAll('#discord-log > li[data-links-view=\"true\"]').length === 3",
                  "the rows with links did not show their links")
            view = page.evaluate(LINK_VIEW_JS)
            shot("3-links-shown")
            check(not view["problems"], f"{label}, the Links view: {view['problems']}")
            rows = {row_id: links for row_id, links in view["rows"]}
            check(sorted(rows) == ["200", "202", "203"], f"{label}: the Links view shows rows {sorted(rows)}")
            check([link[:2] for link in rows["200"]] == [["https://example.com/build/7", "https://example.com/build/7"],
                                                         ["the diff", "https://example.org/diff/7"]],
                  f"{label}: row 200 shows {rows['200']}")
            check([link[:2] for link in rows["202"]] == [["the dashboard", "https://example.com/dash"]],
                  f"{label}: row 202 shows {rows['202']}")
            check(rows["203"][0][1] == LONG_ADDRESS and bool(rows["203"][0][4]),
                  f"{label}: the long address is not whole in its href and cut short on screen: {rows['203']}")
            for links in rows.values():
                for link in links:
                    check(link[2] == "_blank" and link[3] == "noopener noreferrer",
                          f"{label}: {link[0]} does not open in a new tab on its own: {link}")

            # A real tap on a link opens it in a new tab, and the row it is in does not fold.
            anchor = '#discord-log > li[data-id="200"] .row-link[href="https://example.org/diff/7"]'
            x, y = point(anchor)
            hit = page.evaluate(f"() => document.elementFromPoint({x}, {y}) === document.querySelector({json.dumps(anchor)})")
            check(bool(hit), f"{label}: something covers the link to be tapped")
            with context.expect_page(timeout=10_000) as tab:
                tap(anchor)
            opened_page = tab.value
            opened_page.wait_for_load_state()
            check(opened_page.url == "https://example.org/diff/7", f"{label}: the tap opened {opened_page.url}")
            check(page.url.endswith("/voice"), f"{label}: the tap navigated the app itself to {page.url}")
            opened_page.close()
            after = page.evaluate("() => { const row = document.querySelector('#discord-log > li[data-id=\"200\"]');"
                                  " return [row.getAttribute('data-collapsed'), row.getAttribute('data-links-view'),"
                                  " Boolean(row.querySelector('.msg-details'))]; }")
            shot("4-link-tapped")
            check(after[0] == folded and after[1] == "true" and not after[2],
                  f"{label}: the tap on a link also did something to its row: {after}")

            # The glass folds the bar, and Links with it.
            tap("#search-toggle")
            until("() => document.getElementById('links-filter').hidden", "folding the bar left Links on screen")
            check(page.evaluate("() => document.getElementById('links-filter').getAttribute('aria-pressed')") == "false",
                  f"{label}: Links still says it is on")
            until("() => !document.querySelector('#discord-log > li.search-hidden, #discord-log .row-links')",
                  "folding the bar left the Links view up")

            # Over the call view there are no pins: Links stands beside the glass on its own.
            page.click("#view-switch")
            page.wait_for_selector("#pane-voice", state="visible", timeout=5_000)
            tap("#search-toggle")
            until("() => !document.getElementById('links-filter').hidden", "the call view's bar has no Links filter")
            alone = page.evaluate(FILTER_BAR_JS, {"pinned": False, "fieldMin": 140})
            shot("5-call-view-bar")
            check(not alone["problems"], f"{label}, the call view's bar: {alone['problems']}")
            check(not errors, f"{label}, links: the page threw: {errors}")
            check(opened == ["https://example.org/diff/7"], f"{label}: the walk opened {opened}")
            context.close()
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def links_bar_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
                   root_percent: int) -> float:
    """`#211 link-filter`: the open bar with Links on, where a reader's own settings narrow the line.

    A phone in the dark theme at one of `BAR_SCALES`. A real tap opens the bar and another turns Links
    on, which puts the count up with nothing typed, so the bar holds everything it ever holds: the
    field, the count, Links, Pinned and the glass. The glass is still its full size, each filter a 44px
    target clear of its neighbours, and the field still leaves room for a query's text; the Links view
    under the bar shows each link on its own 44px line, none of it under the bar; and the kinds of link
    under Links (`#212 link-filter`) are 44px targets under its icon and inside the screen, clear of the
    bar and the first row. Answers the room the field leaves for its text.
    """
    api = LinkApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    shape = f"{label}, the open bar"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-bar-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625,
                is_mobile=True, has_touch=True, color_scheme="dark",
            )
            page = context.pages[0]
            larger_root_font(page, root_percent)
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{shape}: {why}")

            def tap(selector: str) -> None:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                page.touchscreen.tap(float(found[0]), float(found[1]))

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 5", "the channel did not open")
            tap("#search-toggle")
            until("() => !document.getElementById('links-filter').hidden", "the open bar has no Links filter")
            tap("#links-filter")
            until("() => document.getElementById('links-filter').getAttribute('aria-pressed') === 'true'",
                  "a tap on Links did not turn it on")
            until("() => document.querySelectorAll('#discord-log > li[data-links-view=\"true\"]').length === 3",
                  "the rows with links did not show their links")
            until("() => document.getElementById('search-count').textContent === '3 of 5 loaded'",
                  "the count is not up beside the field")
            glass = page.evaluate("() => { const b = document.getElementById('search-toggle').getBoundingClientRect();"
                                  " return [b.width, b.height]; }")
            check(float(glass[0]) >= GLASS_MIN_PX and float(glass[1]) >= GLASS_MIN_PX,
                  f"{shape}: the glass is {glass[0]}x{glass[1]}px, under {GLASS_MIN_PX}px")
            bar = page.evaluate(FILTER_BAR_JS, {"pinned": True, "fieldMin": 0})
            view = page.evaluate(LINK_VIEW_JS)
            # `#212 link-filter`. The kinds of link under Links, with no GitHub link loaded and so no
            # number on any of them but Other, which is all four links (`#266 link-filter-kinds`):
            # centred under Links where they fit, scrolling where they do not, and inside the screen
            # at every one of these sizes.
            kinds = page.evaluate(KINDS_JS, {"pinned": True})
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"{label}-bar-links-on.png"))
            check(not bar["problems"], f"{shape}, with Links on: {bar['problems']}")
            check(not view["problems"], f"{shape}, the Links view under it: {view['problems']}")
            check(not kinds["problems"], f"{shape}, the kinds of link under Links: {kinds['problems']}")
            check([chip[2] for chip in kinds["chips"]] == ["", "", "", "", "", "4"], f"{shape}: the kinds read {kinds['chips']}")
            shown = sorted(str(row[0]) for row in view["rows"])
            check(shown == ["200", "202", "203"], f"{shape}: the Links view shows rows {shown}")
            check(not errors, f"{shape}: the page threw: {errors}")
            context.close()
            return float(bar["text"])
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def kinds_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
               mobile: bool, root_percent: int) -> str:
    """`#212 link-filter`: the kinds of link under Links, in the dark theme, against a fresh `KindsApi`.

    A real tap on Links brings up the six kinds, all on and numbered — the numbers adding up to every
    link the Links view shows (`#266 link-filter-kinds`) — measured by `KINDS_JS`; their words are
    legible on and off and the two states look different. Real taps then turn PRs, Commits and Actions
    off one by one and the Links view loses those links, row by row, a row with none left going; Tab
    and Enter turn Issues and Files off, each scrolled into view as it takes the focus, which leaves
    Other's links and only them; a reload keeps the choice; turned back on, every link returns. The
    glass takes the row away, and over the call view it is right-aligned under the bar's end. Answers
    how the row was placed over the channel.
    """
    api = KindsApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    shape = f"{label}, the kinds of link"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-kinds-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625 if mobile else 1,
                is_mobile=mobile, has_touch=mobile, color_scheme="dark",
            )
            page = context.pages[0]
            larger_root_font(page, root_percent)
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def shot(name: str) -> None:
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"{label}-kinds-{name}.png"))

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{shape}: {why}")

            def tap(selector: str) -> None:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                if mobile:
                    page.touchscreen.tap(float(found[0]), float(found[1]))
                else:
                    page.mouse.click(float(found[0]), float(found[1]))

            def links_on() -> None:
                tap("#search-toggle")
                until("() => !document.getElementById('links-filter').hidden", "the open bar has no Links filter")
                check(page.evaluate("() => document.getElementById('link-kinds').hidden"),
                      f"{shape}: the kinds are up before Links is on")
                tap("#links-filter")
                until("() => document.getElementById('links-filter').getAttribute('aria-pressed') === 'true'",
                      "a tap on Links did not turn it on")
                until("() => !document.getElementById('link-kinds').hidden", "Links came on without its kinds")

            def links_shown() -> dict[str, list[str]]:
                view = page.evaluate(LINK_VIEW_JS)
                check(not view["problems"], f"{shape}, the Links view: {view['problems']}")
                return {str(row_id): [str(link[1]) for link in links] for row_id, links in view["rows"]}

            def toggle(kind: str, pressed: str) -> None:
                tap(f"#link-kind-{kind}")
                until(f"() => document.getElementById('link-kind-{kind}').getAttribute('aria-pressed') === '{pressed}'",
                      f"a tap on {kind} did not turn it {'on' if pressed == 'true' else 'off'}")

            def key_toggle(kind: str, pressed: str) -> None:
                # `#266 link-filter-kinds`. Tab to the next kind and press Enter, as a keyboard does: the
                # browser scrolls the row to bring the button into view as it takes the focus.
                page.keyboard.press("Tab")
                until(f"() => document.activeElement && document.activeElement.id === 'link-kind-{kind}'",
                      f"Tab did not reach {kind}")
                left, right, row_left, row_right, screen, _scrolled = page.evaluate(KIND_PLACE_JS, f"link-kind-{kind}")
                check(left >= row_left - 0.5 and right <= row_right + 0.5 and left >= -0.5 and right <= screen + 0.5,
                      f"{shape}: {kind}, focused, is not scrolled into view: {left:.1f}..{right:.1f} in a row of"
                      f" {row_left:.1f}..{row_right:.1f} on a {screen}px screen")
                page.keyboard.press("Enter")
                until(f"() => document.getElementById('link-kind-{kind}').getAttribute('aria-pressed') === '{pressed}'",
                      f"Enter on {kind} did not turn it {'on' if pressed == 'true' else 'off'}")

            def counted(chips: list[list[str]]) -> int:
                return sum(int(chip[2] or 0) for chip in chips)

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 6", "the channel did not open")

            # Links on: the six kinds under it, all on, numbered, every link in view — and the numbers
            # add up to every link shown, because every link is of exactly one kind.
            links_on()
            kinds = page.evaluate(KINDS_JS, {"pinned": True})
            shot("1-kinds-on")
            check(not kinds["problems"], f"{shape}, over the channel: {kinds['problems']}")
            check(kinds["chips"] == [["PRs", "true", "13"], ["Commits", "true", "2"], ["Actions", "true", "1"],
                                     ["Issues", "true", "1"], ["Files", "true", "1"], ["Other", "true", "2"]],
                  f"{shape}: the kinds read {kinds['chips']}")
            everything = {
                "200": [f"{REPO}/pull/41", "https://example.com/docs/design.html"],
                "201": [f"{REPO}/actions/runs/123456/job/789", f"{REPO}/commit/abc1234def"],
                "202": [f"{REPO}/commit/0123456789abcdef0123456789abcdef01234567"],
                "203": ["https://example.org/spec", f"{REPO}/issues/7", f"{REPO}/blob/main/docs/spec.md"],
            }
            shown = links_shown()
            check({k: v for k, v in shown.items() if k != "205"} == everything and len(shown.get("205", [])) == 12,
                  f"{shape}: with every kind on, the Links view shows {shown}")
            check(counted(kinds["chips"]) == sum(len(links) for links in shown.values()),
                  f"{shape}: the kinds count {counted(kinds['chips'])} links of the {sum(len(v) for v in shown.values())} shown")

            # PRs off: the pull requests leave their rows, and the row of nothing else goes.
            toggle("pr", "false")
            until("() => !document.querySelector('#discord-log > li[data-id=\"205\"]').offsetParent",
                  "the row holding only pull requests stayed")
            shown = links_shown()
            shot("2-prs-off")
            check(shown == {**everything, "200": ["https://example.com/docs/design.html"]},
                  f"{shape}: with PRs off the Links view shows {shown}")
            on, off = (page.evaluate(KIND_LOOK_JS, f"link-kind-{kind}") for kind in ("commit", "pr"))
            check(on["look"][0] != off["look"][0] and on["look"][2] != off["look"][2] and on["look"][3] != off["look"][3],
                  f"{shape}: on and off are drawn alike: {on['look']} and {off['look']}")
            check("line-through" in off["look"][1] and "line-through" not in on["look"][1],
                  f"{shape}: off is not struck through, or on is: {off['look']}, {on['look']}")
            check(on["contrast"] >= 4.5 and off["contrast"] >= 4.5,
                  f"{shape}: the words on a kind read at {on['contrast']:.2f}:1 on and {off['contrast']:.2f}:1 off")

            # Commits and Actions off too: the documents, the issue and the file.
            toggle("commit", "false")
            toggle("action", "false")
            until("() => document.getElementById('search-count').textContent === '2 of 6 loaded'",
                  "the count does not say what the kinds left")
            shown = links_shown()
            check(shown == {"200": ["https://example.com/docs/design.html"], "203": everything["203"]},
                  f"{shape}: with PRs, Commits and Actions off the Links view shows {shown}")

            # `#266 link-filter-kinds`. Issues and Files off from the keyboard: what is left is Other's,
            # the documents, and nothing else.
            page.focus("#link-kind-action")
            key_toggle("issue", "false")
            key_toggle("file", "false")
            shown = links_shown()
            check(shown == {"200": ["https://example.com/docs/design.html"], "203": ["https://example.org/spec"]},
                  f"{shape}: with every kind but Other off the Links view shows {shown}")
            page.evaluate("() => document.activeElement && document.activeElement.blur()")
            kinds = page.evaluate(KINDS_JS, {"pinned": True})
            shot("3-only-other")
            check(not kinds["problems"], f"{shape}, every kind but Other off: {kinds['problems']}")
            check([chip[:2] for chip in kinds["chips"]] == [["PRs", "false"], ["Commits", "false"], ["Actions", "false"],
                                                           ["Issues", "false"], ["Files", "false"], ["Other", "true"]],
                  f"{shape}: the kinds read {kinds['chips']}")
            stored = page.evaluate("() => localStorage.getItem('vibe-talk.voice.hidden-link-kinds')")
            check(stored == '["pr","commit","action","issue","file"]', f"{shape}: the device kept {stored!r}")

            # A reload keeps the choice; turned back on, every link is back.
            page.reload(wait_until="load")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 6"
                  " && !document.getElementById('pane-discord').hidden", "the reload did not reopen the channel")
            links_on()
            kinds = page.evaluate(KINDS_JS, {"pinned": True})
            check([chip[1] for chip in kinds["chips"]] == ["false", "false", "false", "false", "false", "true"],
                  f"{shape}: after a reload the kinds read {kinds['chips']}")
            check(set(links_shown()) == {"200", "203"}, f"{shape}: after a reload the hidden kinds came back")
            for kind in ("pr", "commit", "action"):
                toggle(kind, "true")
            page.focus("#link-kind-action")
            for kind in ("issue", "file"):
                key_toggle(kind, "true")
            until("() => document.getElementById('search-count').textContent === '5 of 6 loaded'",
                  "turning every kind back on did not bring every row back")
            shown = links_shown()
            check({k: v for k, v in shown.items() if k != "205"} == everything and len(shown.get("205", [])) == 12,
                  f"{shape}: turned back on, the Links view shows {shown}")

            # The glass takes the row away with the bar.
            tap("#search-toggle")
            until("() => document.getElementById('link-kinds').hidden"
                  " && !document.getElementById('screen-main').hasAttribute('data-link-kinds')",
                  "folding the bar left the kinds up")

            # Over the call view Links stands beside the glass: right-aligned under the bar's end.
            page.click("#view-switch")
            page.wait_for_selector("#pane-voice", state="visible", timeout=5_000)
            links_on()
            call = page.evaluate(KINDS_JS, {"pinned": False})
            shot("4-call-view")
            check(not call["problems"], f"{shape}, over the call view: {call['problems']}")
            check(call["placement"] == "right-aligned", f"{shape}: over the call view the kinds are {call['placement']}")
            tap("#search-toggle")
            until("() => document.getElementById('link-kinds').hidden", "folding the bar over the call left the kinds up")
            check(not errors, f"{shape}: the page threw: {errors}")
            context.close()
            return str(kinds["placement"])
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def crowded_kinds_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
                       root_percent: int) -> float:
    """`#212 link-filter`: the row of kinds at its widest — "99+" on every button — on a narrow line.

    A phone in the dark theme, against `CrowdedKindsApi`, at one of `CROWDED_KINDS_SIZES`: Links on by
    real taps, then `KINDS_JS`. With the words free to grow with the type, three of these were 355px
    wide at 150% on a 360px phone and ran off the left of the screen. Six (`#266 link-filter-kinds`)
    are far wider than any phone: the row stays inside the bar and scrolls, a real swipe across it
    brings Other — last, and furthest off — wholly into view, and a real tap then turns it off. Answers
    how wide the six buttons are together.
    """
    api = CrowdedKindsApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    shape = f"{label}, the kinds of link at their widest"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-crowded-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625,
                is_mobile=True, has_touch=True, color_scheme="dark",
            )
            page = context.pages[0]
            larger_root_font(page, root_percent)
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{shape}: {why}")

            def tap(selector: str) -> None:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                page.touchscreen.tap(float(found[0]), float(found[1]))

            cdp = context.new_cdp_session(page)

            def swipe_kinds_left() -> None:
                """One finger, landed near the row's right end and drawn most of the way to its left, then lifted."""
                found = page.evaluate("() => { const r = document.getElementById('link-kinds').getBoundingClientRect();"
                                      " return [r.left, r.right, r.top + r.height / 2]; }")
                start, end, y = float(found[1]) - 12, float(found[0]) + 12, float(found[2])
                steps = 12
                cdp.send("Input.dispatchTouchEvent", {"type": "touchStart", "touchPoints": [{"x": start, "y": y}]})
                for step in range(1, steps + 1):
                    page.wait_for_timeout(16)
                    cdp.send("Input.dispatchTouchEvent", {
                        "type": "touchMove", "touchPoints": [{"x": start + (end - start) * step / steps, "y": y}]})
                page.wait_for_timeout(16)
                cdp.send("Input.dispatchTouchEvent", {"type": "touchEnd", "touchPoints": []})
                page.wait_for_timeout(300)

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 6", "the channel did not open")
            tap("#search-toggle")
            until("() => !document.getElementById('links-filter').hidden", "the open bar has no Links filter")
            tap("#links-filter")
            until("() => !document.getElementById('link-kinds').hidden", "Links came on without its kinds")
            kinds = page.evaluate(KINDS_JS, {"pinned": True})
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"{label}-kinds-crowded.png"))
            check(not kinds["problems"], f"{shape}: {kinds['problems']}")
            check([chip[2] for chip in kinds["chips"]] == ["99+"] * 6, f"{shape}: the kinds read {kinds['chips']}")
            check(bool(kinds["scrolls"]), f"{shape}: six buttons reading 99+ fit the bar, so nothing here scrolls")

            # A swipe, or two, brings Other wholly into view; and a tap on it then turns it off.
            def other_in_view() -> bool:
                left, right, row_left, row_right, screen, _scrolled = page.evaluate(KIND_PLACE_JS, "link-kind-other")
                return bool(left >= row_left - 0.5 and right <= row_right + 0.5 and left >= -0.5 and right <= screen + 0.5)

            check(not other_in_view(), f"{shape}: Other is in view before any swipe")
            for _swipe in range(4):
                if other_in_view():
                    break
                swipe_kinds_left()
            scrolled = page.evaluate(KIND_PLACE_JS, "link-kind-other")[5]
            check(other_in_view(), f"{shape}: a swipe across the kinds did not bring Other into view (scrolled {scrolled}px)")
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"{label}-kinds-crowded-swiped.png"))
            tap("#link-kind-other")
            until("() => document.getElementById('link-kind-other').getAttribute('aria-pressed') === 'false'",
                  "a tap on Other, swiped into view, did not turn it off")
            until("() => document.getElementById('search-count').textContent === '5 of 6 loaded'",
                  "Other off did not take the row of documents away")
            check(not errors, f"{shape}: the page threw: {errors}")
            context.close()
            return float(kinds["span"])
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def empty_walk(chromium: BrowserType, args: argparse.Namespace, label: str, width: int, height: int,
               root_percent: int, mode: str) -> float:
    """`#212 link-filter`, from review: the sentence a list the filters emptied says, where it is read.

    A phone in the dark theme at one of `EMPTY_SIZES`, the channel opened at its newest line, and the
    list emptied by real taps and typing as `mode` says. The sentence must then be wholly on the screen
    below the bar and the kinds of link (`EMPTY_SENTENCE_JS`): the composer below every list kept the
    list overflowing with all its rows gone, so a reader left at the newest line had the sentence under
    the kinds' three buttons — at 412px and 150% type — or, with the keyboard up, above the screen.
    No jump to the newest message is offered past it. Undone the same way, the rows come back with the reader at the newest line again. Answers how far
    the list was scrolled while the sentence was up.
    """
    api: FakeApi = LinklessApi() if mode == "linkless" else EmptyKindsApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    shape = f"{label}, an emptied list ({mode})"
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-empty-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": width, "height": height}, device_scale_factor=2.625,
                is_mobile=True, has_touch=True, color_scheme="dark",
            )
            page = context.pages[0]
            larger_root_font(page, root_percent)
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            def until(script: str, why: str) -> None:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not page.evaluate(script):
                    page.wait_for_timeout(50)
                check(bool(page.evaluate(script)), f"{shape}: {why}")

            def tap(selector: str) -> None:
                found = page.evaluate(f"() => {{ const b = document.querySelector({json.dumps(selector)})"
                                      ".getBoundingClientRect(); return [b.left + b.width / 2, b.top + b.height / 2]; }")
                page.touchscreen.tap(float(found[0]), float(found[1]))

            def pressed(selector: str, value: str, why: str) -> None:
                tap(selector)
                until(f"() => document.querySelector({json.dumps(selector)}).getAttribute('aria-pressed') === '{value}'", why)

            def settled() -> None:
                # Two frames, so a scroll the page made has been laid out and its events delivered.
                page.evaluate("() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))")

            def at_newest() -> bool:
                return bool(page.evaluate("() => { const a = document.getElementById('scroll-area');"
                                          " return a.scrollHeight - a.scrollTop - a.clientHeight <= 24; }"))

            page.goto(f"http://127.0.0.1:{server.server_port}/voice", wait_until="load")
            page.fill("#api-token", TOKEN)
            page.click("#save-token")
            page.wait_for_selector("#search-toggle", state="visible", timeout=10_000)
            page.click("#view-switch")
            until("() => document.querySelectorAll('#discord-log > li[data-id]').length === 4", "the channel did not open")
            settled()
            check(at_newest(), f"{shape}: the channel did not open at its newest line")
            tap("#search-toggle")
            until("() => !document.getElementById('links-filter').hidden", "the open bar has no Links filter")
            if mode != "search":
                pressed("#links-filter", "true", "a tap on Links did not turn it on")
            if mode == "kinds":
                for kind in ("pr", "commit", "action"):
                    pressed(f"#link-kind-{kind}", "false", f"a tap on {kind} did not turn it off")
            elif mode in ("typed", "search"):
                page.fill("#search-field", "nothing to open" if mode == "typed" else "no such words anywhere")
            sentence = EMPTY_SENTENCES[mode]
            until(f"() => document.getElementById('search-empty').textContent.trim() === {json.dumps(sentence)}"
                  " && !document.getElementById('search-empty').hidden", f"the emptied list does not say {sentence!r}")
            settled()
            empty = page.evaluate(EMPTY_SENTENCE_JS)
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"{label}-empty-{mode}.png"))
            check(not empty["problems"], f"{shape}, scrolled to {empty['top']}: {empty['problems']}")
            check(bool(page.evaluate("() => document.getElementById('jump-newest').hidden")),
                  f"{shape}: the jump to the newest message is offered over a list with none shown")

            # Undone the way it was done: the rows are back, at the newest line, and the sentence gone.
            if mode == "kinds":
                pressed("#link-kind-pr", "true", "a tap on PRs did not turn it back on")
            elif mode in ("typed", "search"):
                page.fill("#search-field", "")
            else:
                pressed("#links-filter", "false", "a tap on Links did not turn it off")
            until("() => document.getElementById('search-empty').hidden"
                  " && Boolean(document.querySelector('#discord-log > li[data-id=\"200\"]').offsetParent)",
                  "undoing the filter did not bring the first row back")
            settled()
            check(at_newest(), f"{shape}: the rows came back with the reader short of the newest line")
            check(not errors, f"{shape}: the page threw: {errors}")
            context.close()
            return float(empty["top"])
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


def windows_walk(chromium: BrowserType, args: argparse.Namespace) -> None:
    """`#265 window-channel-memory`: two pages of one browser profile, as two windows of the installed
    app on a desk are — one `localStorage` between them and a `sessionStorage` each. Each is left on
    its own channel and reloaded, and comes back on it rather than on the one the other chose last;
    a third, opened after both, opens on the channel chosen last. Real storage, a real reload."""
    api = DockApi()
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(api))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/voice"
    listed_first, listed_second = str(LONG_CHANNEL["id"]), str(CHANNEL["id"])
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-talk-chrome-windows-") as profile:
            context = chromium.launch_persistent_context(
                profile, headless=True, executable_path=args.browser_executable,
                viewport={"width": 1280, "height": 800},
            )
            errors: list[str] = []

            def channel(page: Page) -> str:
                return str(page.evaluate("() => document.getElementById('discord-channel').value"))

            def read_for(act: Callable[[], object], page: Page, expected: str, why: str) -> None:
                """Do `act`, and wait for the channel read it leads to: one of `expected`, and only that.

                A reopen reads its channel once the server has accepted the token, so by then the page
                has taken the server's channel list and the picker holds what it will keep."""
                with api.lock:
                    api.requests.clear()
                act()
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not api.reads():
                    page.wait_for_timeout(50)
                page.wait_for_timeout(200)
                reads = api.reads()
                check(bool(reads), f"windows: {why}: no channel was read")
                read = sorted({r.split("/")[4] for r in reads})
                check(read == [expected], f"windows: {why}: read {read}, expected {expected}")
                check(channel(page) == expected, f"windows: {why}: the picker is on {channel(page)}, not {expected}")

            def opened() -> Page:
                page = context.new_page()
                page.on("pageerror", lambda error: errors.append(str(error)))
                return page

            def choose(page: Page, channel_id: str, why: str) -> None:
                read_for(lambda: page.select_option("#discord-channel", channel_id), page, channel_id, why)

            # The first window signs in, opens the channel — the first listed — and changes to the second.
            first = context.pages[0]
            first.on("pageerror", lambda error: errors.append(str(error)))
            first.goto(url, wait_until="load")
            first.fill("#api-token", TOKEN)
            first.click("#save-token")
            first.wait_for_selector("#view-switch", state="visible", timeout=10_000)
            read_for(lambda: first.click("#view-switch"), first, listed_first, "the first window's channel")
            choose(first, listed_second, "the first window's choice")
            # A second window opens on that, the last choice, and is put on the first listed.
            second = opened()
            read_for(lambda: second.goto(url, wait_until="load"), second, listed_second,
                     "a new window, on the channel chosen last")
            choose(second, listed_first, "the second window's choice")
            # Each reloads, on its own channel: the first window not on the one the second chose after it.
            read_for(lambda: first.reload(wait_until="load"), first, listed_second, "the first window's reload")
            read_for(lambda: second.reload(wait_until="load"), second, listed_first, "the second window's reload")
            # A third window, opened after both, on the channel chosen last; a reload did not choose.
            third = opened()
            read_for(lambda: third.goto(url, wait_until="load"), third, listed_first,
                     "a window opened after both, on the channel chosen last")
            check(not errors, f"windows: the page threw: {errors}")
            context.close()
    finally:
        api.stopping.set()
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    raise SystemExit(main())
