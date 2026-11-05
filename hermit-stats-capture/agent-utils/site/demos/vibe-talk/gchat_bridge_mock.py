#!/usr/bin/env python3
"""A loopback stand-in for a Google Chat bridge, plus a canned Messages API, for vibe-talk demos.

vibe-talk reads chat through Discord's HTTP API or through a bridge that speaks it. This file is
such a bridge, serving ONE fictional Google Chat space from memory: a small team of coding agents
on a made-up project ("acme-widgets"), supervised by its owner. It exists to record demo videos of
the vibe-talk page against the real vibe-talk server, without any chat vendor or model vendor.

Two listeners, both loopback only:

  * the bridge, on --bridge-port, answering the subset of the Discord HTTP API vibe-talk uses with
    `thread_api = "bridge"` (see vibe-talk/README.md, "Thread backend contract"):
        GET  /api/v10/users/@me
        GET  /api/v10/channels/{id}/messages?limit=&before=&after=
        POST /api/v10/channels/{id}/messages
        GET  /api/v10/channels/{id}/timeline?view=main|threads|flat|thread&thread_id=&before=&limit=
        GET  /api/v10/channels/{id}/threads/{thread_id}/messages/{message_id}
        POST /api/v10/channels/{id}/threads/{thread_id}/messages
    Message text is Google Chat markup (*bold*, <https://url|label> links). Every top-level
    message heads its own thread, as on Google Chat.

  * a Messages API, on --llm-port, answering `POST /v1/messages` in the Anthropic wire format with
    canned, hand-written summaries and thread names, chosen by keywords in the fenced message.

A small control plane on the bridge port drives the script while a recorder films the page:
        POST /_demo/reset             reseed the conversation, timed relative to now
        POST /_demo/cue/{name}        add a scripted group of messages (see CUES), spaced in time
        GET  /_demo/state             what vibe-talk has posted, and how many messages exist
A post from the page that answers the approval question triggers the agents' scripted answers by
itself, a few seconds later, as a real coordinator would.

When --hint-url and --ingest-token-file are given, every new message is followed by a content-free
change hint to vibe-talk (`POST /api/v1/live/hints`), so the server reads the bridge at once rather
than at its next poll. That is the same acceleration a real bridge adapter would provide.

Standard library only. Nothing here contacts any network beyond 127.0.0.1.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
import time
import urllib.request
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import TypedDict
from urllib.parse import parse_qs, unquote, urlsplit
from zoneinfo import ZoneInfo

Json = dict[str, object]


class _SpecBase(TypedDict):
    who: str
    text: str


class Spec(_SpecBase, total=False):
    """One scripted message, before it is given an id and a time."""

    key: str
    ago: int
    thread: str
    reply_to: str
    literal: bool  # text from the page: no "{time}" template to fill


class Held(TypedDict):
    """One message as this bridge holds it."""

    id: str
    ms: int
    who: str
    content: str
    thread_id: str
    root_id: str
    reply_to: str | None


CHANNEL_ID = "1300000000000000001"
SPACE = "spaces/AAQAacmeWidgets"
DISCORD_EPOCH_MS = 1420070400000
ZONE = ZoneInfo("America/New_York")

# The owner's own account. vibe-talk posts as this account too: its token's first segment encodes
# this id (see vibe-talk src/discord/mod.rs, self_user_id_from_token), so the page draws both the
# owner's Google Chat messages and the ones sent from the app as the owner's.
USERS: dict[str, Json] = {
    "owner": {"id": "1500000000000000001", "username": "Sam", "global_name": "Sam", "bot": False},
    # Every agent posts through one shared app account and names itself in a [tag] at the head of
    # the message; vibe-talk lifts that tag into the row's header ("rich headers").
    "agents": {"id": "1500000000000000010", "username": "acme-agents",
               "global_name": "acme-agents", "bot": True},
}

# ---------------------------------------------------------------------------------------------
# The scripted conversation. Fictional: no real people, companies, hosts or projects. `ago` is
# minutes before the reset; `thread` names the key of the root a reply belongs to; `reply_to` the
# key of a message it quotes. "{time}" is replaced with the message's own clock time.

PLAN = (
    "[coordinator] Plan for today, {time}.\n"
    "Picked up Sam's list and leased three work slots with `wrkslots`:\n"
    "- *worker-1*: fix the flaky `checkout_total` test (issue 412)\n"
    "- *worker-2*: CSV export on the Reports page (issue 398)\n"
    "- *worker-3*: bump the build toolchain to 1.86 and re-run `make validate`\n"
    "Each worker has its own checkout through `agentctl`, so nobody steps on anybody else. "
    "I'll post here as each task lands, and I'll ask before anything goes to main."
)

CHECKOUT = (
    "[worker-1] Flaky checkout test fixed, {time}.\n"
    "Root cause: the cart fixture shared one clock between parallel tests, so a total computed "
    "a second before midnight UTC sometimes rounded into the next day's tax rate. Each test now "
    "gets its own fake clock. *200 consecutive runs green*, and `make validate` is green. "
    "Branch: <https://git.example.com/acme-widgets/compare/fix-checkout-clock|fix-checkout-clock>"
)

REPORT = (
    "[worker-2] CSV export report, {time}.\n"
    "*Summary:* saved reports can now be exported as CSV, from the Reports page and from the "
    "API. Ready for review.\n"
    "\n"
    "*What changed*\n"
    "- New *Export CSV* button on every saved report. It streams rows, so a 200,000-row report "
    "downloads in about 4 seconds without being held in memory.\n"
    "- `GET /api/reports/{{id}}/export?format=csv`, with the same column order as the table on "
    "screen.\n"
    "- Dates are ISO 8601 in UTC. Money keeps two decimals and no thousands separators, so "
    "spreadsheets read it as numbers.\n"
    "\n"
    "*Tests*\n"
    "| Suite | Result |\n"
    "|---|---|\n"
    "| unit | 214 passed, 14 new |\n"
    "| end-to-end | 31 passed |\n"
    "| `make validate` | green in 3m12s |\n"
    "\n"
    "*Open questions*\n"
    "1. Should exports include archived rows? I left them out, to match the screen.\n"
    "2. Read-only users can see the button; the API already lets them export. Keep it?\n"
    "\n"
    "Change: <https://git.example.com/acme-widgets/pull/398|change 398> · "
    "CI: <https://ci.example.com/acme-widgets/runs/5531|run 5531>"
)

APPROVAL = (
    "[worker-3] Approval needed for the toolchain bump, {time}.\n"
    "The bump to 1.86 is ready in slot ws-3. `make validate` passes, two new lint warnings are "
    "fixed, and a clean build is 9% faster. It touches only `rust-toolchain.toml` and the two "
    "lint fixes. *OK to land it on main?* I'll wait for your go-ahead before merging."
)

SEED: list[Spec] = [
    {"key": "kickoff", "ago": 62, "who": "owner",
     "text": "Morning! Today: the flaky checkout test, CSV export for reports, and the toolchain "
             "bump. I'm out most of the day, so keep me posted here and I'll answer from my phone."},
    {"key": "plan", "ago": 58, "who": "agents", "text": PLAN},
    {"key": "plan-r1", "ago": 57, "who": "agents", "thread": "plan",
     "text": "[worker-1] Taking issue 412 in slot ws-1."},
    {"key": "plan-r2", "ago": 56, "who": "agents", "thread": "plan",
     "text": "[worker-2] CSV export is mine, slot ws-2."},
    {"key": "plan-r3", "ago": 55, "who": "agents", "thread": "plan",
     "text": "[worker-3] Toolchain bump in slot ws-3. I'll run the full `make validate` before "
             "asking."},
    {"key": "checkout", "ago": 44, "who": "agents", "text": CHECKOUT},
    {"key": "checkout-r1", "ago": 40, "who": "owner", "thread": "checkout",
     "text": "Great find. Merge it when CI is green."},
    {"key": "checkout-r2", "ago": 39, "who": "agents", "thread": "checkout",
     "text": "[coordinator] Merged, {time}. worker-1 is writing a regression test next."},
    {"key": "report", "ago": 31, "who": "agents", "text": REPORT},
    {"key": "report-r1", "ago": 24, "who": "owner", "thread": "report",
     "text": "Nice work. Leave archived rows out for now, and yes, read-only users keep the button."},
    {"key": "report-r2", "ago": 23, "who": "agents", "thread": "report",
     "text": "[worker-2] Noted. No change needed, so it's ready for review as it stands."},
    {"key": "approval", "ago": 13, "who": "agents", "text": APPROVAL},
    {"key": "status", "ago": 9, "who": "agents",
     "text": "[coordinator] Status, {time}. worker-1 merged, worker-2 is waiting for review, and "
             "worker-3 needs your OK on the toolchain bump above."},
]

# Scripted groups added while recording: (seconds after the cue, message). A message may quote
# `reply_to` (a seed key, or "$post" for the post that triggered the cue) or join `thread`.
CUES: dict[str, list[tuple[float, Spec]]] = {
    "worker-report": [
        (0.0, {"key": "regression", "who": "agents",
               "text": "[worker-1] Regression test added, {time}. It pins the fixture clock to "
                       "23:59:59 UTC, so the rounding bug cannot come back. `make validate` is "
                       "green."}),
        (6.0, {"key": "release-slot", "who": "agents",
               "text": "[coordinator] Thanks. worker-1's slot is back in the pool "
                       "(`wrkslots release ws-1`)."}),
    ],
    "approval-landed": [
        (3.0, {"key": "landed", "who": "agents", "reply_to": "$post",
               "text": "[worker-3] Landed, {time}. The toolchain bump is on main and "
                       "`make validate` is green on the merged commit."}),
        (8.3, {"key": "wrapup", "who": "agents",
               "text": "[coordinator] Confirmed. All three of today's tasks are done; the CSV "
                       "export goes out with tomorrow's release."}),
    ],
    "report-ack": [
        (3.0, {"key": "report-ack", "who": "agents", "thread": "report",
               "text": "[worker-2] Will do. Tagged for tomorrow's release, and I've released "
                       "slot ws-2."}),
    ],
}

# Which posts from the page set off a cue: a post that quotes, or is in the thread of, this key.
TRIGGERS: dict[str, str] = {"approval": "approval-landed", "report": "report-ack"}

# ---------------------------------------------------------------------------------------------
# Canned answers for the Messages API. Matched by a phrase in the message being summarised (or in
# the thread being named); the first match wins.

SUMMARIES: list[tuple[str, str]] = [
    ("Plan for today", "The coordinator split today's work across three agents, each in its own "
                       "slot, and will ask you before anything lands on main."),
    ("Flaky checkout test fixed", "Worker-1 traced the flaky checkout test to a clock shared "
                                  "between parallel tests and fixed it; validation is green."),
    ("CSV export report", "Worker-2 finished CSV export for saved reports with all tests "
                          "passing, and asks whether to include archived rows and keep the "
                          "button for read-only users."),
    ("Approval needed", "Worker-3 asks your approval to land the toolchain bump, which passes "
                        "validation and builds nine percent faster."),
]

THREAD_NAMES: list[tuple[str, str, str]] = [
    ("Plan for today", "todays-work-plan",
     "Each worker claimed one of today's three tasks and a work slot."),
    ("Flaky checkout test fixed", "checkout-clock-fix",
     "The flaky checkout test fix was approved and merged."),
    ("CSV export report", "csv-export-review",
     "The CSV export is ready for review; archived rows stay out."),
    ("Approval needed", "toolchain-bump-approval",
     "Worker-3 is waiting for approval to land the toolchain bump."),
]


def log(*parts: object) -> None:
    print(time.strftime("%H:%M:%S"), *parts, file=sys.stderr, flush=True)


def snowflake(ms: int, seq: int) -> str:
    return str(((ms - DISCORD_EPOCH_MS) << 22) | (seq & 0xFFF))


def iso(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat(
        timespec="milliseconds").replace("+00:00", "Z")


def clock(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, tz=ZONE).strftime("%-I:%M %p ET")


def limit_of(query: dict[str, str]) -> int:
    try:
        return max(1, min(100, int(query.get("limit") or 50)))
    except ValueError:
        return 50


class Hinter:
    """Tells vibe-talk the channel changed, so its next read of the bridge is immediate."""

    def __init__(self, url: str | None, token: str | None) -> None:
        self.url = url
        self.token = token

    def send(self) -> None:
        url, token = self.url, self.token
        if not url or not token:
            return

        def post() -> None:
            request = urllib.request.Request(
                url, data=json.dumps({"channel_id": CHANNEL_ID}).encode(), method="POST",
                headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"})
            try:
                with urllib.request.urlopen(request, timeout=5) as response:
                    response.read()
            except OSError as error:  # a missed hint only delays delivery to the next poll
                log(f"hint failed: {error}")

        threading.Thread(target=post, daemon=True).start()


class Store:
    """The space's messages, in memory, oldest first."""

    def __init__(self, hint: Hinter) -> None:
        self.lock = threading.RLock()
        self.hint = hint
        self.seq = 0
        self.messages: list[Held] = []
        self.by_key: dict[str, Held] = {}
        self.posts: list[Json] = []
        self.generation = 0

    # -- building ------------------------------------------------------------------------------

    def _new(self, spec: Spec, ms: int, *, post_key: str | None = None) -> Held:
        self.seq += 1
        mid = snowflake(ms, self.seq)
        thread_root = self._resolve(spec["thread"], post_key) if "thread" in spec else None
        target = self._resolve(spec["reply_to"], post_key) if "reply_to" in spec else None
        text = spec["text"]
        if not spec.get("literal"):
            text = text.replace("{time}", clock(ms)).replace("{{", "{").replace("}}", "}")
        message: Held = {
            "id": mid,
            "ms": ms,
            "who": spec["who"],
            "content": text,
            "thread_id": thread_root["thread_id"] if thread_root else f"{SPACE}/threads/{mid}",
            "root_id": thread_root["id"] if thread_root else mid,
            "reply_to": target["id"] if target else None,
        }
        self.messages.append(message)
        if "key" in spec:
            self.by_key[spec["key"]] = message
        return message

    def _resolve(self, key: str, post_key: str | None) -> Held | None:
        if key == "$post":
            key = post_key or ""
        return self.by_key.get(key)

    def key_of(self, message: Held) -> str | None:
        return next((key for key, held in self.by_key.items() if held is message), None)

    def reset(self) -> None:
        with self.lock:
            self.generation += 1
            self.messages = []
            self.by_key = {}
            self.posts = []
            now = int(time.time() * 1000)
            for index, spec in enumerate(SEED):
                # Spaced by more than a second, so no two messages share an instant.
                self._new(spec, now - spec.get("ago", 0) * 60_000 + index * 1500)
        log(f"reset: {len(self.messages)} messages seeded")

    def add(self, spec: Spec, *, post_key: str | None = None, hint: bool = True) -> Held:
        with self.lock:
            message = self._new(spec, int(time.time() * 1000), post_key=post_key)
        log(f"added {message['id']} ({spec.get('key', 'post')})")
        if hint:
            # vibe-talk spaces hinted reads at least five seconds apart, so the page's own posts
            # (which the page already holds) are not hinted: a read spent on one would delay the
            # agents' scripted answer that follows a few seconds later.
            self.hint.send()
        return message

    def cue(self, name: str, *, post_key: str | None = None) -> bool:
        steps = CUES.get(name)
        if steps is None:
            return False
        generation = self.generation

        def run() -> None:
            start = time.monotonic()
            for delay, spec in steps:
                time.sleep(max(0.0, start + delay - time.monotonic()))
                if self.generation != generation:
                    return
                self.add(spec, post_key=post_key)

        threading.Thread(target=run, daemon=True).start()
        log(f"cue {name} started")
        return True

    # -- the wire format -----------------------------------------------------------------------

    def _replies(self, thread_id: str) -> list[Held]:
        return [m for m in self.messages if m["thread_id"] == thread_id and m["id"] != m["root_id"]]

    def wire(self, m: Held) -> Json:
        replies = self._replies(m["thread_id"])
        return {
            "id": m["id"],
            "channel_id": CHANNEL_ID,
            "type": 0,
            "author": dict(USERS[m["who"]]),
            "timestamp": iso(m["ms"]),
            "content": m["content"],
            "thread": {
                "id": m["thread_id"],
                "root_message_id": m["root_id"],
                "is_root": m["id"] == m["root_id"],
                "reply_count": len(replies),
                "reply_count_exact": True,
            },
            "message_reference": {"message_id": m["reply_to"]} if m["reply_to"] else None,
            # No reactions: the recording host's Chromium has no colour emoji font, so a chip would
            # be drawn as an empty box. An empty list says the bridge knows there are none.
            "reactions": [],
        }

    def root_of(self, thread_id: str) -> Held | None:
        return next((m for m in self.messages if m["thread_id"] == thread_id
                     and m["id"] == m["root_id"]), None)

    def summary(self, thread_id: str) -> Json | None:
        root = self.root_of(thread_id)
        if root is None:
            return None
        replies = self._replies(thread_id)
        newest = max([root, *replies], key=lambda m: m["ms"])
        return {
            "id": thread_id,
            "root": self.wire(root),
            "title": root["content"].split("\n", 1)[0][:80],
            "reply_count": len(replies),
            "reply_count_exact": True,
            "updated_at": iso(newest["ms"]),
        }

    def page(self, rows: list[Held], query: dict[str, str]) -> tuple[list[Held], bool, str | None]:
        """Oldest first, at most `limit`, older than `before` when given."""
        limit = limit_of(query)
        before = query.get("before")
        if before and before.isdigit():
            rows = [m for m in rows if int(m["id"]) < int(before)]
        has_more = len(rows) > limit
        rows = rows[-limit:]
        return rows, has_more, (rows[0]["id"] if has_more and rows else None)


# ---------------------------------------------------------------------------------------------


def make_bridge_handler(store: Store) -> type[BaseHTTPRequestHandler]:
    class Bridge(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002 - stdlib's name
            pass  # quiet; anything unexpected is logged by the handlers below

        def reply(self, status: int, body: object) -> None:
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def body(self) -> Json | None:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            try:
                value: object = json.loads(raw or b"{}")
            except ValueError:
                return None
            return value if isinstance(value, dict) else None

        def route(self) -> tuple[list[str], dict[str, str]]:
            parts = urlsplit(self.path)
            segments = [unquote(s) for s in parts.path.split("/") if s]
            query = {k: v[-1] for k, v in parse_qs(parts.query).items()}
            return segments, query

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            segments, query = self.route()
            with store.lock:
                if segments == ["_demo", "state"]:
                    return self.reply(200, {"posts": store.posts, "messages": len(store.messages)})
                if segments[:2] != ["api", "v10"]:
                    return self.reply(404, {"message": "not found"})
                rest = segments[2:]
                if rest == ["users", "@me"]:
                    owner = USERS["owner"]
                    return self.reply(200, {"id": owner["id"], "username": owner["username"]})
                if len(rest) >= 2 and rest[0] == "channels" and rest[1] != CHANNEL_ID:
                    return self.reply(404, {"message": "Unknown Channel", "code": 10003})
                if len(rest) == 3 and rest[2] == "messages":
                    return self.messages(query)
                if len(rest) == 3 and rest[2] == "timeline":
                    return self.timeline(query)
                if len(rest) == 6 and rest[2] == "threads" and rest[4] == "messages":
                    found = next((m for m in store.messages if m["id"] == rest[5]
                                  and m["thread_id"] == rest[3]), None)
                    if found is None:
                        return self.reply(404, {"message": "Unknown Message", "code": 10008})
                    return self.reply(200, store.wire(found))
            log(f"unhandled GET {self.path}")
            self.reply(404, {"message": "not found"})

        def messages(self, query: dict[str, str]) -> None:
            rows = list(store.messages)
            after = query.get("after")
            if after and after.isdigit():
                rows = [m for m in rows if int(m["id"]) > int(after)][:limit_of(query)]
            else:
                rows, _, _ = store.page(rows, query)
            # Discord's order: newest first.
            self.reply(200, [store.wire(m) for m in reversed(rows)])

        def timeline(self, query: dict[str, str]) -> None:
            view = query.get("view", "main")
            if query.get("after"):
                # This bridge issues no next_after, so vibe-talk never sends one; say so if it does.
                return self.reply(400, {"message": "this bridge issues no forward cursors"})
            page: Json = {"messages": [], "threads": [], "thread": None, "has_threads": True,
                          "has_more": False, "next_before": None, "notice": None}
            rows: list[Held] = []
            if view == "main":
                rows = [m for m in store.messages if m["id"] == m["root_id"]]
            elif view == "flat":
                rows = list(store.messages)
            elif view == "thread":
                thread_id = query.get("thread_id", "")
                summary = store.summary(thread_id)
                if summary is None:
                    return self.reply(404, {"message": "Unknown Thread"})
                rows = [m for m in store.messages if m["thread_id"] == thread_id]
                page["thread"] = summary
            elif view == "threads":
                summaries = [store.summary(m["thread_id"]) for m in store.messages
                             if m["id"] == m["root_id"]]
                listed = [s for s in summaries if s is not None and s["reply_count"] != 0]
                listed.sort(key=lambda s: str(s["updated_at"]))
                page["threads"] = listed
                return self.reply(200, page)
            else:
                return self.reply(400, {"message": f"unknown view {view}"})
            shown, has_more, next_before = store.page(rows, query)
            page.update(messages=[store.wire(m) for m in shown], has_more=has_more,
                        next_before=next_before)
            self.reply(200, page)

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            segments, _ = self.route()
            body = self.body()
            if body is None:
                return self.reply(400, {"message": "body is not a JSON object"})
            if segments == ["_demo", "reset"]:
                store.reset()
                return self.reply(200, {"ok": True})
            if len(segments) == 3 and segments[:2] == ["_demo", "cue"]:
                ok = store.cue(segments[2])
                return self.reply(200 if ok else 404, {"ok": ok})
            rest = segments[2:] if segments[:2] == ["api", "v10"] else []
            if len(rest) >= 2 and rest[0] == "channels" and rest[1] != CHANNEL_ID:
                return self.reply(404, {"message": "Unknown Channel", "code": 10003})
            if len(rest) == 3 and rest[2] == "messages":
                return self.post(body, None)
            if len(rest) == 5 and rest[2] == "threads" and rest[4] == "messages":
                return self.post(body, rest[3])
            log(f"unhandled POST {self.path}")
            self.reply(404, {"message": "not found"})

        def post(self, body: Json, thread_id: str | None) -> None:
            content = str(body.get("content") or "").strip()
            if not content:
                return self.reply(400, {"message": "Cannot send an empty message", "code": 50006})
            reference_object = body.get("message_reference")
            reference = (str(reference_object.get("message_id") or "") or None
                         if isinstance(reference_object, dict) else None)
            nonce = body.get("nonce")
            with store.lock:
                if nonce:
                    earlier = next((p for p in store.posts if p.get("nonce") == nonce), None)
                    if earlier is not None:  # the same request again: answer with the same message
                        again = next(m for m in store.messages if m["id"] == earlier["id"])
                        return self.reply(200, store.wire(again))
                key = f"post-{len(store.posts) + 1}"
                spec: Spec = {"who": "owner", "text": content, "key": key, "literal": True}
                root_key = None
                if thread_id is not None:
                    root = store.root_of(thread_id)
                    root_key = store.key_of(root) if root else None
                    if root_key is None:
                        return self.reply(404, {"message": "Unknown Thread"})
                    spec["thread"] = root_key
                quoted_key = None
                if reference:
                    quoted = next((m for m in store.messages if m["id"] == reference), None)
                    quoted_key = store.key_of(quoted) if quoted else None
                    if quoted_key:
                        spec["reply_to"] = quoted_key
                message = store.add(spec, hint=False)
                if reference and not quoted_key:
                    message["reply_to"] = reference
                store.posts.append({"id": message["id"], "content": content, "nonce": nonce,
                                    "thread_id": thread_id, "reply_to": reference})
                answer = store.wire(message)
            log(f"post from the page: {content[:60]!r}")
            for target, cue in TRIGGERS.items():
                if target in (quoted_key, root_key):
                    store.cue(cue, post_key=key)
            self.reply(200, answer)

    return Bridge


def canned(prompt: str, system: str) -> str:
    if "name chat threads" in system or "The thread to name" in prompt:
        for phrase, slug, sentence in THREAD_NAMES:
            if phrase in prompt:
                return f"{slug}\n{sentence}"
        return "team-discussion\nThe team discussed the day's work."
    target = prompt.split("The message to summarise:", 1)[-1]
    for phrase, summary in SUMMARIES:
        if phrase in target:
            return summary
    return "An agent posted an update on today's work."


def make_llm_handler(delay: float) -> type[BaseHTTPRequestHandler]:
    class Llm(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002 - stdlib's name
            pass

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's API
            length = int(self.headers.get("Content-Length") or 0)
            try:
                parsed: object = json.loads(self.rfile.read(length) or b"{}")
            except ValueError:
                parsed = {}
            request: Json = parsed if isinstance(parsed, dict) else {}
            if urlsplit(self.path).path != "/v1/messages":
                data = b'{"type":"error","error":{"type":"not_found_error","message":"no route"}}'
                self.send_response(404)
            else:
                system = str(request.get("system") or "")
                turns = request.get("messages")
                prompt = "".join(str(turn.get("content") or "") for turn in turns
                                 if isinstance(turn, dict)) if isinstance(turns, list) else ""
                text = canned(prompt, system)
                # A real model takes a moment; the page measures and shows the round trip, and an
                # instant answer would advertise a latency no deployment has.
                time.sleep(delay)
                log(f"llm: {text[:70]!r}")
                data = json.dumps({
                    "id": "msg_demo", "type": "message", "role": "assistant",
                    "model": request.get("model", "demo"), "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": text}],
                    "usage": {"input_tokens": len(prompt) // 4, "output_tokens": len(text) // 4},
                }).encode()
                self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    return Llm


def main() -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").split("\n\n")[0])
    parser.add_argument("--bridge-port", type=int, default=19311,
                        help="loopback port for the bridge and its /_demo control plane "
                             "(default 19311)")
    parser.add_argument("--llm-port", type=int, default=19312,
                        help="loopback port for the canned Messages API (default 19312)")
    parser.add_argument("--llm-delay", type=float, default=0.8,
                        help="seconds the canned Messages API waits before answering, so the "
                             "latency the page reports is plausible (default 0.8)")
    parser.add_argument("--hint-url", default=None,
                        help="vibe-talk's POST /api/v1/live/hints URL; omit to rely on polling")
    parser.add_argument("--ingest-token-file", default=None,
                        help="file holding vibe-talk's ingest token, for --hint-url")
    args = parser.parse_args()
    token = None
    if args.ingest_token_file:
        with open(args.ingest_token_file, encoding="utf-8") as handle:
            token = handle.read().strip()
    store = Store(Hinter(args.hint_url, token))
    store.reset()
    bridge = ThreadingHTTPServer(("127.0.0.1", args.bridge_port), make_bridge_handler(store))
    llm = ThreadingHTTPServer(("127.0.0.1", args.llm_port), make_llm_handler(args.llm_delay))
    for server in (bridge, llm):
        server.daemon_threads = True
    threading.Thread(target=llm.serve_forever, daemon=True).start()
    log(f"bridge on 127.0.0.1:{args.bridge_port}, messages API on 127.0.0.1:{args.llm_port}")
    try:
        bridge.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
