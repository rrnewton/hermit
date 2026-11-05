#!/usr/bin/env python3
"""Measure what the /voice page costs a battery: CPU and memory, scenario by scenario, build by build.

`#226 page-energy-profile`. The owner keeps vibe-talk open as an installed app on a desktop and on a
phone and wants to know it is not draining either. A battery cannot be read from here, so this
measures the proxy that drives one: CPU time spent by the browser's processes (and by the server),
plus the renderer's own accounting of script, style and layout work, and the heap and DOM size.

WHAT IT RUNS
============

Everything is local and synthetic. Each job starts:

  * a fake chat bridge, in this process, speaking the HTTP shape of a Discord-compatible bridge with
    normalized threads (`thread_api = "bridge"`): one channel of a few hundred messages with threads
    and replies, emoji reactions, GitHub and other links, Markdown lists and code, and reply
    references. Deterministic: the same seed draws the same channel for every build;
  * the vibe-talk server binary under test (`--build LABEL=PATH`, any number of them) on a free
    loopback port, pointed at that bridge, with live delivery by adapter push (`[ingest]`), the
    way a bridge adapter feeds it; nothing reaches a real chat service or a voice vendor;
  * Chromium in its new headless mode, driven over the DevTools protocol directly rather than
    through Playwright, because Playwright holds every page visible and the hidden scenario needs a
    page the browser itself believes is backgrounded (a minimized window). The browser is pinned
    with `taskset` to a few cores so a loaded host moves it about less.

Two device profiles: `mobile` (412x915, touch, DPR 2.625, an Android user agent) and `desktop`
(1280x800, mouse, DPR 1).

SCENARIOS
=========

  a  idle, visible, no traffic                         (--idle-seconds, default 300)
  b  idle, window minimized (document hidden), two back-to-back windows of --hidden-seconds
     (default 300 each), then live traffic while still hidden, then the return to visible
  c  live traffic: one message every --live-every seconds for --live-seconds in Main, All and an
     open thread, pushed through the server's live route as an adapter would
  d  scrolling the whole of All up (paging older history in) and back down
  e  view switches All -> Main -> thread x20, read-mode cycling, opening and closing the row menu
  f  the Voice pane open and idle                      (--voice-seconds, default 180)
  g  memory: heap, DOM nodes and listeners after c and after each of two rounds of e (part of the
     `ceg` job, which runs c, e, e in one page)
  server  the server process alone, with no page, then with one idle page (part of job `a`)
  e  (not in the default set) scenario e alone in a fresh page, for comparing many builds
  p  (not in the default set) the cost of one background refresh with nothing new: the page's
     own poll entry point called --polls times in Main and in All with all history loaded

Every segment records: CPU seconds by Chromium process type and in total (from /proc, so it counts
compositor, raster and network threads too), the server's CPU seconds, the renderer's
Performance.getMetrics deltas (TaskDuration, ScriptDuration, LayoutDuration, RecalcStyleDuration,
LayoutCount, RecalcStyleCount), long tasks, and at checkpoints the heap and node counts after a
forced garbage collection.

`--census` adds an instrumented pass that wraps timers, animation frames, observers and fetch in the
page and lists everything that ran, with call counts and creation sites, plus the CSS animations
running at each sample. `--cpu-profile` adds a sampled V8 profile per segment and reports the top
functions by self and inclusive time. Both perturb what they measure, so neither is folded into
the CPU numbers.

USAGE
=====

Build the binaries first (web/ is compiled into the server), then, for example:

    scripts/energy_profile.py \\
        --build base=/tmp/base/target/release/vibe-talk \\
        --build now=target/release/vibe-talk \\
        --repeats 3 --parallel 6 --out /tmp/energy

    scripts/energy_profile.py --report-only /tmp/energy     # re-aggregate saved results
    scripts/energy_profile.py --report-only /tmp/energy --compare base,now   # side by side, with changes

A full run (two builds, two profiles, every scenario, three repeats) is a few CPU-hours of
Chromium and roughly an hour of wall time at --parallel 8, which is why it is not in the validate
DAG. `--quick` shortens every segment for a smoke run of the harness itself.

Needs Python `websockets` (>= 12, for its sync client) and a Chromium: Playwright's bundled one is
found automatically, or pass --chrome.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import os
import random
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Callable, cast
from urllib.parse import parse_qs, unquote, urlsplit

Json = dict[str, object]


def obj(value: object) -> Json:
    """A JSON object, or a loud failure: the shapes here are this script's own or Chromium's."""
    assert isinstance(value, dict), f"expected a JSON object, got {type(value).__name__}"
    return value


def num(value: object) -> float:
    assert isinstance(value, (int, float)), f"expected a number, got {type(value).__name__}"
    return float(value)


def text(value: object) -> str:
    assert isinstance(value, str), f"expected a string, got {type(value).__name__}"
    return value


def items(value: object) -> list[object]:
    assert isinstance(value, list), f"expected a JSON array, got {type(value).__name__}"
    return value

CHANNEL_ID = "1420000000000000001"
CHANNEL_LABEL = "lead team"
BOT_ID = "1420000000000000009"
OWNER_ID = "1420000000000000002"
BOT_TOKEN = base64.urlsafe_b64encode(BOT_ID.encode()).decode().rstrip("=") + ".energy.profile-never-sent"
READ_TOKEN = "energy-profile-read-token-000000000000"
WRITE_TOKEN = "energy-profile-write-token-11111111111"
INGEST_TOKEN = "energy-profile-ingest-token-2222222222"
DISCORD_EPOCH_MS = 1_420_070_400_000
CLK_TCK = os.sysconf("SC_CLK_TCK")
ANDROID_UA = ("Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) "
              "Chrome/151.0.0.0 Mobile Safari/537.36")
@dataclass(frozen=True)
class Profile:
    width: int
    height: int
    dpr: float
    mobile: bool
    touch: bool


PROFILES: dict[str, Profile] = {
    "mobile": Profile(width=412, height=915, dpr=2.625, mobile=True, touch=True),
    "desktop": Profile(width=1280, height=800, dpr=1.0, mobile=False, touch=False),
}
METRIC_KEYS = ("TaskDuration", "ScriptDuration", "LayoutDuration", "RecalcStyleDuration",
               "LayoutCount", "RecalcStyleCount")
SNAPSHOT_KEYS = ("JSHeapUsedSize", "JSHeapTotalSize", "Nodes", "JSEventListeners", "Documents",
                 "LayoutObjects")


# =================================================================================================
# The synthetic channel.
# =================================================================================================

AGENTS = [
    ("codex-eng", "1420000000000000101"), ("claude-integ", "1420000000000000102"),
    ("codex-review", "1420000000000000103"), ("build-bot", "1420000000000000104"),
    ("claude-qa", "1420000000000000105"),
]
EMOJI = ["👍", "🎉", "👀", "✅", "🚀", "❤️", "😂", "🙏"]
REPOS = ["acme/widgets", "acme/gateway", "acme/infra"]
WORDS = ("runner cache token retry budget deploy canary rollback flake ordering branch review "
         "landing tip green queue arm64 nightly migration schema index latency p99 alert dashboard "
         "fixture harness timeout lock shard replica checkpoint rebase artifact release tag").split()


def snowflake(ms: int, seq: int) -> str:
    return str(((ms - DISCORD_EPOCH_MS) << 22) | (seq & 0x3FFFFF))


def iso(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat().replace("+00:00", "Z")


def sentence(rng: random.Random, words: int) -> str:
    text = " ".join(rng.choice(WORDS) for _ in range(words))
    return text[0].upper() + text[1:] + "."


def gh_link(rng: random.Random) -> str:
    repo = rng.choice(REPOS)
    kind = rng.random()
    if kind < 0.45:
        return f"https://github.com/{repo}/pull/{rng.randint(100, 9999)}"
    if kind < 0.7:
        return f"https://github.com/{repo}/commit/{rng.getrandbits(160):040x}"
    if kind < 0.85:
        return f"https://github.com/{repo}/actions/runs/{rng.randint(10**9, 10**10)}"
    return f"https://github.com/{repo}/issues/{rng.randint(10, 999)}"


def content_for(rng: random.Random) -> str:
    """One message body: short, Markdown, linked or long, in the proportions an agent channel has."""
    roll = rng.random()
    if roll < 0.30:
        return sentence(rng, rng.randint(6, 18))
    if roll < 0.45:
        return f"{sentence(rng, rng.randint(5, 12))} {gh_link(rng)}"
    if roll < 0.60:
        items = "\n".join(f"- **{rng.choice(WORDS)}**: {sentence(rng, rng.randint(4, 10))}"
                          f"{' `' + rng.choice(WORDS) + '()`' if rng.random() < 0.4 else ''}"
                          for _ in range(rng.randint(3, 7)))
        return f"{sentence(rng, rng.randint(5, 10))}\n\n{items}\n\n{sentence(rng, 6)}"
    if roll < 0.70:
        code = "\n".join(f"    let {rng.choice(WORDS)} = {rng.choice(WORDS)}({rng.randint(0, 99)});"
                         for _ in range(rng.randint(3, 9)))
        return f"{sentence(rng, 8)}\n\n```rust\nfn check() {{\n{code}\n}}\n```\n\n{sentence(rng, 5)}"
    if roll < 0.80:
        links = " ".join(gh_link(rng) for _ in range(rng.randint(2, 4)))
        return (f"{sentence(rng, 9)} See {links} and https://docs.example.com/{rng.choice(WORDS)} "
                f"for the details. *{rng.choice(WORDS)}* is ~{rng.randint(2, 40)}% faster.")
    if roll < 0.92:
        return " ".join(sentence(rng, rng.randint(10, 22)) for _ in range(rng.randint(4, 9)))
    return "\n\n".join(" ".join(sentence(rng, rng.randint(12, 24)) for _ in range(4))
                       for _ in range(rng.randint(3, 5)))


def reactions_for(rng: random.Random) -> list[Json] | None:
    if rng.random() >= 0.22:
        return None
    out = [{"count": rng.randint(1, 3), "emoji": {"id": None, "name": e}}
           for e in rng.sample(EMOJI, rng.randint(1, 3))]
    if rng.random() < 0.15:
        out.append({"count": 1, "emoji": {"id": "998877665544332211", "name": "shipit"}})
    return out


@dataclass
class Msg:
    """One message: the fields this script reasons about, and the wire form a bridge answers with."""

    id: str
    ms: int
    author: Json
    content: str
    seq: int
    rev: int
    thread_id: str | None = None
    is_root: bool = False
    root_id: str | None = None
    reply_count: int = 0
    reply_to: str | None = None
    reactions: list[Json] | None = None

    def wire(self) -> Json:
        out: Json = {"id": self.id, "channel_id": CHANNEL_ID, "author": self.author,
                     "timestamp": iso(self.ms), "content": self.content}
        if self.thread_id is not None:
            out["thread"] = {"id": self.thread_id, "root_message_id": self.root_id, "is_root": self.is_root,
                             "reply_count": self.reply_count, "reply_count_exact": True}
        if self.reply_to is not None:
            out["message_reference"] = {"message_id": self.reply_to}
        if self.reactions is not None:
            out["reactions"] = self.reactions
        return out

    def normalized(self) -> Json:
        """The server's own message shape, as an adapter pushes it to the live route."""
        out: Json = {
            "id": self.id, "channel_id": CHANNEL_ID,
            "author": text(self.author.get("global_name") or self.author["username"]),
            "author_id": self.author["id"], "author_is_bot": self.author["bot"], "timestamp": iso(self.ms),
            "content": self.content, "reply_to": self.reply_to,
        }
        if self.thread_id is not None:
            out["thread"] = obj(self.wire()["thread"])
        if self.reactions is not None:
            shown: list[Json] = []
            for reaction in self.reactions:
                emoji = obj(reaction["emoji"])
                item: Json = {"emoji": emoji["name"], "count": reaction["count"]}
                if emoji["id"]:
                    item.update(custom=True, custom_id=emoji["id"])
                shown.append(item)
            out["reactions"] = shown
        return out


@dataclass
class Thread:
    root: Msg
    title: str
    replies: list[Msg] = field(default_factory=list)


@dataclass
class Channel:
    """The channel's messages, oldest first, and its threads; answered in a bridge's wire shape."""

    messages: list[Msg] = field(default_factory=list)
    threads: dict[str, Thread] = field(default_factory=dict)
    lock: threading.Lock = field(default_factory=threading.Lock)
    seq: int = 0
    rng: random.Random = field(default_factory=lambda: random.Random(2260))

    def author(self, roll: float) -> Json:
        if roll < 0.18:
            return {"id": BOT_ID, "username": "vibe-talk", "bot": True}
        if roll < 0.23:
            return {"id": OWNER_ID, "username": "owner", "global_name": "Owner", "bot": False}
        name, ident = AGENTS[int(roll * 1000) % len(AGENTS)]
        return {"id": ident, "username": name, "bot": True}

    def make(self, ms: int, thread: Thread | None, reply_to: str | None) -> Msg:
        self.seq += 1
        msg = Msg(id=snowflake(ms, self.seq), ms=ms, author=self.author(self.rng.random()),
                  content=content_for(self.rng), seq=self.seq, rev=self.seq, reply_to=reply_to,
                  reactions=reactions_for(self.rng))
        if thread is not None:
            msg.thread_id, msg.root_id = thread.root.thread_id, thread.root.id
            msg.reply_count = thread.root.reply_count
        return msg

    @classmethod
    def generate(cls, main_count: int = 170, threads: int = 30, now_ms: int | None = None) -> "Channel":
        ch = cls()
        rng = ch.rng
        now_ms = now_ms or int(time.time() * 1000) - 60_000
        start = now_ms - 3 * 86_400_000
        times = sorted(rng.randint(start, now_ms - 3_600_000) for _ in range(main_count))
        roots = set(rng.sample(range(main_count - 1), threads - 1)) | {main_count - 1}
        drafts: list[tuple[int, str]] = []  # (when, thread id)
        mains: list[Msg] = []
        for i, ms in enumerate(times):
            recent = [m.id for m in mains[-10:]]
            reply = rng.choice(recent) if recent and rng.random() < 0.10 else None
            msg = ch.make(ms, None, reply)
            mains.append(msg)
            if i in roots:
                tid = f"spaces/AAAAenergy/threads/t{i:03d}"
                count = 45 if i == main_count - 1 else rng.choice([2, 3, 3, 4, 5, 6, 8, 10, 12, 15, 20, 30])
                msg.thread_id, msg.is_root, msg.root_id, msg.reply_count = tid, True, msg.id, count
                ch.threads[tid] = Thread(root=msg, title=sentence(rng, 4).rstrip("."))
                span = rng.randint(600_000, 8 * 3_600_000)
                for k in range(count):
                    drafts.append((min(now_ms, ms + int(span * (k + 1) / count)), tid))
        replies: list[Msg] = []
        for ms, tid in sorted(drafts, key=lambda d: d[0]):
            entry = ch.threads[tid]
            recent = [m.id for m in entry.replies[-8:]] or [entry.root.id]
            reply = rng.choice(recent) if rng.random() < 0.12 else None
            msg = ch.make(ms, entry, reply)
            entry.replies.append(msg)
            replies.append(msg)
        ch.messages = sorted(mains + replies, key=lambda m: int(m.id))
        for n, msg in enumerate(ch.messages):
            msg.seq = msg.rev = n + 1
        ch.seq = len(ch.messages)
        return ch

    # --- the bridge's reads -----------------------------------------------------------------------

    def summary(self, tid: str) -> Json:
        entry = self.threads[tid]
        last = entry.replies[-1] if entry.replies else entry.root
        return {"id": tid, "root": entry.root.wire(), "title": entry.title,
                "reply_count": len(entry.replies), "reply_count_exact": True, "updated_at": iso(last.ms)}

    @staticmethod
    def in_view(msg: Msg, view: str, thread_id: str | None) -> bool:
        if view == "flat":
            return True
        if view == "thread":
            return msg.thread_id is not None and msg.thread_id == thread_id
        return msg.thread_id is None or msg.is_root

    def timeline(self, query: dict[str, list[str]]) -> tuple[int, Json]:
        view = query.get("view", ["main"])[0]
        limit = max(1, min(100, int(query.get("limit", ["50"])[0])))
        thread_id = query.get("thread_id", [""])[0] or None
        before = query.get("before", [""])[0]
        after = query.get("after", [""])[0]
        with self.lock:
            thread = self.summary(thread_id) if view == "thread" and thread_id in self.threads else None
            if view == "thread" and thread is None:
                return 404, {"message": "unknown thread"}
            base: Json = {"threads": [], "thread": thread, "has_threads": True, "notice": None,
                          "next_after": f"a{self.seq}", "as_of": iso(int(time.time() * 1000))}
            if after:
                try:
                    since = int(after.lstrip("a"))
                except ValueError:
                    return 410, {"message": "cursor expired"}
                # COMPLETE, as a bridge with a change record answers: every message created or
                # changed since the cursor (a root whose reply count moved included), and for the
                # threads view every thread that moved.
                delta: Json = {"more": False, "complete": True, "deleted": [], "removed_threads": []}
                if view == "threads":
                    moved = [t for t, entry in self.threads.items() if entry.root.rev > since]
                    return 200, {**base, "messages": [], "threads": [self.summary(t) for t in moved],
                                 "has_more": False, "next_before": None, "delta": delta}
                changed = [m.wire() for m in self.messages if m.rev > since and self.in_view(m, view, thread_id)]
                return 200, {**base, "messages": changed, "has_more": False, "next_before": None, "delta": delta}
            if view == "threads":
                ordered = sorted(self.threads, key=lambda t: self.threads[t].replies[-1].ms
                                 if self.threads[t].replies else self.threads[t].root.ms, reverse=True)
                if before:
                    end = int(before.lstrip("b"))
                    begin = max(0, end - limit)
                    chosen, more = ordered[begin:end], begin > 0
                else:
                    chosen, more = ordered[:limit], len(ordered) > limit
                return 200, {**base, "messages": [], "threads": [self.summary(t) for t in chosen],
                             "has_more": more, "next_before": f"b{limit}" if more and not before else None}
            rows = [m for m in self.messages if self.in_view(m, view, thread_id)]
            end = int(before.lstrip("b")) if before else len(rows)
            begin = max(0, end - limit)
            return 200, {**base, "messages": [m.wire() for m in rows[begin:end]],
                         "has_more": begin > 0, "next_before": f"b{begin}" if begin > 0 else None}

    def flat_page(self, query: dict[str, list[str]]) -> list[Json]:
        limit = max(1, min(100, int(query.get("limit", ["50"])[0])))
        before = query.get("before", [""])[0]
        after = query.get("after", [""])[0]
        with self.lock:
            rows = self.messages
            if before:
                rows = [m for m in rows if int(m.id) < int(before)]
            if after:
                rows = [m for m in rows if int(m.id) > int(after)][:limit]
            else:
                rows = rows[-limit:]
            return [m.wire() for m in reversed(rows)]

    def find(self, message_id: str) -> Json | None:
        with self.lock:
            return next((m.wire() for m in self.messages if m.id == message_id), None)

    def busiest_thread(self) -> str:
        """The newest thread, which is also the one with the longest history (45 replies)."""
        return max(self.threads, key=lambda t: self.threads[t].root.seq)

    # --- live traffic -----------------------------------------------------------------------------

    def append(self, thread_id: str | None) -> Json:
        """A new message, into the channel or into a thread; answered in the server's normalized shape."""
        with self.lock:
            entry = self.threads[thread_id] if thread_id is not None else None
            if entry is not None:
                entry.root.reply_count = len(entry.replies) + 1
            msg = self.make(int(time.time() * 1000), entry, None)
            self.messages.append(msg)
            if entry is not None:
                entry.replies.append(msg)
                entry.root.rev = self.seq
            return msg.normalized()


def bridge_handler(channel: Channel, counts: dict[str, int]) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def reply(self, status: int, payload: object) -> None:
            body = b"" if status == 204 else json.dumps(payload).encode()
            self.send_response(status)
            if status != 204:
                self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def route(self, method: str) -> None:
            parts = urlsplit(self.path)
            path = unquote(parts.path)
            query = parse_qs(parts.query)
            length = int(self.headers.get("Content-Length") or 0)
            if length:
                self.rfile.read(length)
            key = re.sub(r"/\d{6,}", "/{id}", re.sub(r"/threads/[^/]+/[^/]+/[^/]+", "/threads/{tid}", path))
            if "view" in query:
                key += f"?view={query['view'][0]}{'&after' if 'after' in query else ''}{'&before' if 'before' in query else ''}"
            counts[f"{method} {key}"] = counts.get(f"{method} {key}", 0) + 1
            if path == "/users/@me":
                return self.reply(200, {"id": BOT_ID, "username": "vibe-talk", "bot": True})
            m = re.fullmatch(r"/channels/([^/]+)(/.*)?", path)
            if not m or m.group(1) != CHANNEL_ID:
                return self.reply(404, {"message": "Unknown Channel", "code": 10003})
            rest = m.group(2) or ""
            if method == "GET" and rest == "/timeline":
                status, payload = channel.timeline(query)
                return self.reply(status, payload)
            if method == "GET" and rest == "/messages":
                return self.reply(200, channel.flat_page(query))
            found = re.fullmatch(r"(?:/threads/.+)?/messages/(\d+)", rest)
            if method == "GET" and found:
                msg = channel.find(found.group(1))
                return self.reply(200 if msg else 404, msg or {"message": "Unknown Message"})
            if method == "POST" and rest == "/read":
                return self.reply(204, None)
            if method == "GET" and rest == "":
                return self.reply(200, {"id": CHANNEL_ID, "name": CHANNEL_LABEL, "type": 0})
            return self.reply(404, {"message": f"not served by the energy-profile bridge: {method} {rest}"})

        def do_GET(self) -> None:  # noqa: N802
            self.route("GET")

        def do_POST(self) -> None:  # noqa: N802
            self.route("POST")

        def log_message(self, _format: str, *_args: object) -> None:
            pass

    return Handler


# =================================================================================================
# Processes and CPU accounting.
# =================================================================================================

def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def proc_cpu(pid: int) -> float:
    """utime + stime of a process, all its threads, live and exited, in seconds."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / CLK_TCK
    except (OSError, IndexError, ValueError):
        return 0.0


def descendants(root: int) -> dict[int, str]:
    """Every live descendant of `root` (and root itself), labelled by Chromium process type."""
    parents: dict[int, int] = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                parents[int(entry)] = int(f.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
    found = {root}
    changed = True
    while changed:
        changed = False
        for pid, ppid in parents.items():
            if ppid in found and pid not in found:
                found.add(pid)
                changed = True
    out: dict[int, str] = {}
    for pid in found:
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                # Chromium rewrites its own title into one space-separated string.
                argv = f.read().replace(b"\0", b" ").split()
        except OSError:
            continue
        kind = "browser"
        for arg in argv:
            if arg.startswith(b"--type="):
                kind = arg[7:].decode()
                if kind == "utility":
                    sub = next((a for a in argv if a.startswith(b"--utility-sub-type=")), b"")
                    kind = "utility:" + sub.split(b"=", 1)[1].decode().split(".")[-1] if sub else kind
                break
        if kind == "browser" and pid != root:
            kind = "other"
        out[pid] = kind
    return out


class CpuMeter:
    """CPU seconds of a process tree, by kind, across an interval. Processes that die mid-interval
    are counted up to their last sample; ones born mid-interval from zero."""

    def __init__(self, root: int) -> None:
        self.root = root
        self.last: dict[int, tuple[str, float]] = {}
        self.total_by_kind: dict[str, float] = {}

    def sample(self) -> dict[int, tuple[str, float]]:
        return {pid: (kind, proc_cpu(pid)) for pid, kind in descendants(self.root).items()}

    def start(self) -> None:
        self.last = self.sample()

    def stop(self) -> dict[str, float]:
        now = self.sample()
        by_kind: dict[str, float] = {}
        for pid, (kind, cpu) in now.items():
            before = self.last.get(pid, (kind, 0.0))[1]
            by_kind[kind] = by_kind.get(kind, 0.0) + max(0.0, cpu - before)
        by_kind["total"] = sum(v for k, v in by_kind.items())
        return {k: round(v, 3) for k, v in by_kind.items()}


# =================================================================================================
# A small synchronous DevTools client.
# =================================================================================================

class CdpError(RuntimeError):
    pass


class Cdp:
    def __init__(self, ws_url: str) -> None:
        from websockets.sync.client import connect

        self.ws = connect(ws_url, max_size=None, ping_interval=None, open_timeout=30)
        self.next_id = 0
        self.session: str | None = None
        self.exceptions: list[str] = []

    def send(self, method: str, params: Json | None = None, *, browser: bool = False,
             timeout: float = 120.0) -> Json:
        self.next_id += 1
        ident = self.next_id
        msg: Json = {"id": ident, "method": method, "params": params or {}}
        if self.session and not browser:
            msg["sessionId"] = self.session
        self.ws.send(json.dumps(msg))
        deadline = time.monotonic() + timeout
        while True:
            raw = self.ws.recv(timeout=max(0.1, deadline - time.monotonic()))
            reply = json.loads(raw)
            if reply.get("id") == ident:
                if "error" in reply:
                    raise CdpError(f"{method}: {reply['error']}")
                return obj(reply.get("result", {}))
            if reply.get("method") == "Runtime.exceptionThrown":
                details = reply["params"]["exceptionDetails"]
                said = details.get("exception", {}).get("description") or details.get("text")
                self.exceptions.append(str(said)[:300])

    def drain(self) -> None:
        """Read whatever events queued while nothing was asked, so the socket never backs up."""
        while True:
            try:
                raw = self.ws.recv(timeout=0)
            except TimeoutError:
                return
            reply = json.loads(raw)
            if reply.get("method") == "Runtime.exceptionThrown":
                self.exceptions.append(str(reply["params"]["exceptionDetails"].get("text"))[:300])

    def eval(self, expression: str, *, await_promise: bool = False, timeout: float = 60.0) -> object:
        result = self.send("Runtime.evaluate", {"expression": expression, "returnByValue": True,
                                                "awaitPromise": await_promise}, timeout=timeout)
        if "exceptionDetails" in result:
            raise CdpError(f"evaluate failed: {obj(result['exceptionDetails']).get('text')} in {expression[:120]}")
        return obj(result.get("result", {})).get("value")

    def eval_num(self, expression: str) -> float:
        return num(self.eval(expression))

    def wait_for(self, expression: str, timeout: float = 20.0, interval: float = 0.2) -> object:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.eval(expression)
            if value:
                return value
            time.sleep(interval)
        raise CdpError(f"timed out after {timeout}s waiting for: {expression[:160]}")

    def metrics(self) -> dict[str, float]:
        return {text(obj(m)["name"]): num(obj(m)["value"]) for m in items(self.send("Performance.getMetrics")["metrics"])}

    def close(self) -> None:
        try:
            self.ws.close()
        except Exception:  # noqa: BLE001 - closing is best effort
            pass


# What every measured page carries: a long-task counter, nothing else.
LONGTASK_JS = r"""
(() => {
  window.__energy = {longtasks: 0, longtaskMs: 0};
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) { __energy.longtasks++; __energy.longtaskMs += e.duration; }
    }).observe({type: 'longtask', buffered: true});
  } catch (_e) {}
})();
"""

# The census: what runs, how often, and from where. Only in --census passes.
CENSUS_JS = r"""
(() => {
  const C = window.__census = {timers: {}, raf: {}, observers: {}, fetches: {}, listeners: {}};
  const site = () => {
    const lines = (new Error().stack || '').split('\n').slice(3, 6);
    return lines.map((l) => l.trim().replace(/^at /, '').replace(/https?:\/\/[^/]+\//, '')
      .replace(/\?[^:)]*/, '')).join(' < ');
  };
  const bump = (table, key, field, n = 1) => {
    const row = table[key] || (table[key] = {});
    row[field] = (row[field] || 0) + n;
    return row;
  };
  const wrapTimer = (name) => {
    const orig = window[name];
    window[name] = function (fn, delay, ...rest) {
      const key = `${name} ${typeof fn === 'function' ? (fn.name || '(anon)') : 'string'} @ ${site()}`;
      const row = bump(C.timers, key, 'armed');
      row.delay = delay || 0;
      if (typeof fn !== 'function') return orig.call(this, fn, delay, ...rest);
      return orig.call(this, function (...args) {
        const t0 = performance.now();
        try { return fn.apply(this, args); } finally {
          bump(C.timers, key, 'fired'); bump(C.timers, key, 'ms', performance.now() - t0);
        }
      }, delay, ...rest);
    };
  };
  wrapTimer('setTimeout'); wrapTimer('setInterval');
  const origRaf = window.requestAnimationFrame;
  window.requestAnimationFrame = function (fn) {
    const key = `rAF ${fn.name || '(anon)'} @ ${site()}`;
    bump(C.raf, key, 'armed');
    return origRaf.call(this, (ts) => {
      const t0 = performance.now();
      try { return fn(ts); } finally { bump(C.raf, key, 'fired'); bump(C.raf, key, 'ms', performance.now() - t0); }
    });
  };
  for (const name of ['ResizeObserver', 'IntersectionObserver', 'MutationObserver', 'PerformanceObserver']) {
    const Orig = window[name];
    if (typeof Orig !== 'function') continue;
    window[name] = class extends Orig {
      constructor(fn, ...rest) {
        const key = `${name} @ ${site()}`;
        bump(C.observers, key, 'created');
        super(function (...args) {
          const t0 = performance.now();
          try { return fn.apply(this, args); } finally {
            bump(C.observers, key, 'fired'); bump(C.observers, key, 'entries', (args[0] || []).length || 0);
            bump(C.observers, key, 'ms', performance.now() - t0);
          }
        }, ...rest);
      }
    };
  }
  const origFetch = window.fetch;
  window.fetch = function (input, init) {
    const url = typeof input === 'string' ? input : (input && input.url) || String(input);
    const path = url.replace(/^https?:\/\/[^/]+/, '').replace(/\?.*/, '').replace(/\/\d{6,}/g, '/{id}');
    const q = (url.split('?')[1] || '').split('&').filter((p) => /^(view|after|before)=/.test(p))
      .map((p) => p.replace(/=.+/, (v) => (/^=(main|flat|thread|threads)$/.test(v) ? v : '=…'))).join('&');
    bump(C.fetches, `${(init && init.method) || 'GET'} ${path}${q ? '?' + q : ''}`, 'n');
    return origFetch.apply(this, arguments);
  };
  const origAdd = EventTarget.prototype.addEventListener;
  const origRemove = EventTarget.prototype.removeEventListener;
  EventTarget.prototype.addEventListener = function (type, fn, opts) {
    const who = this === window ? 'window' : this === document ? 'document'
      : this instanceof Element ? this.tagName.toLowerCase() + (this.id ? '#' + this.id : this.className ? '.' + String(this.className).split(' ')[0] : '') : (this.constructor && this.constructor.name) || '?';
    bump(C.listeners, `${who} ${type}`, 'added');
    return origAdd.call(this, type, fn, opts);
  };
  EventTarget.prototype.removeEventListener = function (type, fn, opts) {
    const who = this === window ? 'window' : this === document ? 'document'
      : this instanceof Element ? this.tagName.toLowerCase() + (this.id ? '#' + this.id : this.className ? '.' + String(this.className).split(' ')[0] : '') : (this.constructor && this.constructor.name) || '?';
    bump(C.listeners, `${who} ${type}`, 'removed');
    return origRemove.call(this, type, fn, opts);
  };
  C.animations = {};
  C.sampleAnimations = () => {
    for (const a of document.getAnimations()) {
      const t = a.effect && a.effect.target;
      const name = a.animationName || a.transitionProperty || a.constructor.name;
      const where = t ? (t.id ? '#' + t.id : t.tagName.toLowerCase() + '.' + String(t.className).split(' ')[0]) : '?';
      const visible = t && t.getClientRects().length > 0;
      bump(C.animations, `${name} on ${where} (${a.playState}${visible ? '' : ', not rendered'})`, 'samples');
    }
  };
})();
"""


@dataclass
class Rig:
    """One server, one bridge, one browser, one page."""

    server_bin: str
    profile: str
    cpus: str | None
    chrome: str
    census: bool = False
    workdir: Path = field(default_factory=lambda: Path(tempfile.mkdtemp(prefix="energy-profile-")))
    channel: Channel = field(default_factory=Channel.generate)
    bridge_counts: dict[str, int] = field(default_factory=dict)
    server: subprocess.Popen[bytes] | None = None
    browser: subprocess.Popen[str] | None = None
    cdp: Cdp | None = None
    port: int = 0
    target_id: str = ""
    window_id: int = 0

    # --- bring-up ---------------------------------------------------------------------------------

    def start_bridge(self) -> str:
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), bridge_handler(self.channel, self.bridge_counts))
        httpd.daemon_threads = True
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.httpd = httpd
        return f"http://127.0.0.1:{httpd.server_port}"

    def start_server(self) -> None:
        bridge = self.start_bridge()
        self.port = free_port()
        config = self.workdir / "vibe-talk.toml"
        config.write_text(f"""
[server]
bind = "127.0.0.1:{self.port}"

[discord]
provider_name = "Google Chat"
thread_api = "bridge"
upstream_read_marks = true
bot_token = "{BOT_TOKEN}"
api_base = "{bridge}"
owner_user_id = "{OWNER_ID}"
default_fetch_limit = 50
max_fetch_limit = 100
live_poll_seconds = 0

[auth]
read_token = "{READ_TOKEN}"
write_token = "{WRITE_TOKEN}"

[ingest]
token = "{INGEST_TOKEN}"

[[channels]]
id = "{CHANNEL_ID}"
label = "{CHANNEL_LABEL}"
writable = true

[read_aloud]
backend = "browser"

[storage]
path = "{self.workdir}/state/vibe-talk.sqlite3"
""")
        argv = [self.server_bin, "--config", str(config), "--skip-startup-probe"]
        if self.cpus:
            argv = ["taskset", "-c", self.cpus, *argv]
        env = {k: v for k, v in os.environ.items() if not k.startswith("VIBE_TALK_")}
        env["RUST_LOG"] = "warn"
        self.server_log = open(self.workdir / "server.log", "wb")
        self.server = subprocess.Popen(argv, cwd=self.workdir, stdout=self.server_log,
                                       stderr=subprocess.STDOUT, env=env)
        for _ in range(240):
            if self.server.poll() is not None:
                raise RuntimeError(f"server exited: {(self.workdir / 'server.log').read_text()[-2000:]}")
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/healthz", timeout=2).read()
                return
            except (urllib.error.URLError, OSError):
                time.sleep(0.25)
        raise RuntimeError("server never answered /healthz")

    def server_cpu(self) -> float:
        return proc_cpu(self.server.pid) if self.server else 0.0

    def api(self, method: str, path: str, body: Json | None = None, token: str = WRITE_TOKEN) -> object:
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, method=method,
                                         headers={"Authorization": f"Bearer {token}",
                                                  "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read()
            return json.loads(raw) if raw else None

    def start_browser(self) -> None:
        prof = PROFILES[self.profile]
        argv = [self.chrome, "--headless=new", "--remote-debugging-port=0",
                f"--user-data-dir={self.workdir / 'chrome'}", "--no-first-run", "--no-default-browser-check",
                "--disable-sync", "--disable-extensions", "--disable-component-update",
                "--disable-default-apps", "--mute-audio", "--password-store=basic",
                "--use-mock-keychain", "--disable-features=Translate,OptimizationHints,MediaRouter",
                f"--window-size={prof.width},{prof.height}", "about:blank"]
        if self.cpus:
            argv = ["taskset", "-c", self.cpus, *argv]
        self.browser = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        assert self.browser.stderr is not None
        ws_url = None
        deadline = time.monotonic() + 60
        for line in self.browser.stderr:
            found = re.search(r"DevTools listening on (ws://\S+)", line)
            if found:
                ws_url = found.group(1)
                break
            if time.monotonic() > deadline:
                break
        if not ws_url:
            raise RuntimeError("Chromium never printed its DevTools address")
        stderr = self.browser.stderr
        threading.Thread(target=lambda: [None for _ in stderr], daemon=True).start()
        cdp = self.cdp = Cdp(ws_url)
        page: Json | None = None
        for _ in range(150):  # a loaded host can answer before the first tab exists
            targets = items(cdp.send("Target.getTargets", browser=True)["targetInfos"])
            page = next((obj(t) for t in targets if obj(t)["type"] == "page"), None)
            if page:
                break
            time.sleep(0.2)
        if page is None:
            page = {"targetId": cdp.send("Target.createTarget", {"url": "about:blank"}, browser=True)["targetId"]}
        self.target_id = text(page["targetId"])
        cdp.session = text(cdp.send("Target.attachToTarget", {"targetId": self.target_id, "flatten": True},
                                    browser=True)["sessionId"])
        self.window_id = int(num(cdp.send("Browser.getWindowForTarget", {"targetId": self.target_id},
                                          browser=True)["windowId"]))
        cdp.send("Page.enable")
        cdp.send("Runtime.enable")
        cdp.send("Performance.enable", {"timeDomain": "threadTicks"})
        cdp.send("Emulation.setDeviceMetricsOverride", {
            "width": prof.width, "height": prof.height, "deviceScaleFactor": prof.dpr,
            "mobile": prof.mobile})
        if prof.touch:
            cdp.send("Emulation.setTouchEmulationEnabled", {"enabled": True, "maxTouchPoints": 5})
            cdp.send("Emulation.setUserAgentOverride", {"userAgent": ANDROID_UA, "platform": "Android"})
        cdp.send("Emulation.setEmulatedMedia", {"features": [{"name": "prefers-color-scheme", "value": "dark"}]})
        cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": LONGTASK_JS})
        if self.census:
            cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": CENSUS_JS})

    def open_channel(self) -> None:
        """Sign in, switch from the Voice pane to the channel, and wait for its rows."""
        cdp = self.cdp
        assert cdp is not None
        url = f"http://127.0.0.1:{self.port}/voice"
        cdp.send("Page.navigate", {"url": url})
        cdp.wait_for("document.readyState === 'complete'", 30)
        cdp.eval(f"localStorage.setItem('vibe-talk.token', {json.dumps(WRITE_TOKEN)})")
        cdp.send("Page.reload")
        time.sleep(0.5)
        cdp.wait_for("document.readyState === 'complete' && !document.getElementById('screen-main').hidden", 30)
        cdp.wait_for("document.getElementById('discord-channel').options.length > 0", 30)
        if cdp.eval("document.getElementById('pane-discord').hidden"):
            self.tap("#view-switch")
        cdp.wait_for("document.querySelectorAll('#discord-log > li').length > 10", 30)
        self.choose_view("main")
        time.sleep(2)

    # --- input ------------------------------------------------------------------------------------

    def tap(self, selector: str) -> None:
        cdp = self.cdp
        assert cdp is not None
        box = cdp.eval(f"""(() => {{ const n = document.querySelector({json.dumps(selector)});
            if (!n) return null; n.scrollIntoView({{block: 'nearest'}}); const r = n.getBoundingClientRect();
            return [r.left + r.width / 2, r.top + r.height / 2, r.width]; }})()""")
        if not box or num(items(box)[2]) == 0:
            raise CdpError(f"nothing to tap at {selector}")
        x, y = num(items(box)[0]), num(items(box)[1])
        if PROFILES[self.profile].touch:
            point = [{"x": x, "y": y}]
            cdp.send("Input.dispatchTouchEvent", {"type": "touchStart", "touchPoints": point})
            cdp.send("Input.dispatchTouchEvent", {"type": "touchEnd", "touchPoints": []})
        else:
            for kind in ("mouseMoved", "mousePressed", "mouseReleased"):
                cdp.send("Input.dispatchMouseEvent", {"type": kind, "x": x, "y": y, "button": "left",
                                                      "clickCount": 1})

    def scroll(self, up: bool, distance: float = 450) -> None:
        """One reader's scroll gesture toward older (`up`) or newer messages: a finger dragged 450px
        on the phone, 120px wheel notches (10 for 450px) on the desk, at a 60 Hz cadence."""
        cdp = self.cdp
        assert cdp is not None
        prof = PROFILES[self.profile]
        x = prof.width / 2
        if prof.touch:
            y0, y1 = (prof.height * 0.25, prof.height * 0.25 + distance) if up else \
                (prof.height * 0.25 + distance, prof.height * 0.25)
            steps = 20
            cdp.send("Input.dispatchTouchEvent", {"type": "touchStart", "touchPoints": [{"x": x, "y": y0}]})
            for i in range(1, steps + 1):
                cdp.send("Input.dispatchTouchEvent", {"type": "touchMove",
                                                      "touchPoints": [{"x": x, "y": y0 + (y1 - y0) * i / steps}]})
                time.sleep(0.016)
            cdp.send("Input.dispatchTouchEvent", {"type": "touchEnd", "touchPoints": []})
        else:
            for _ in range(max(1, round(distance / 45))):
                cdp.send("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": x, "y": prof.height / 2,
                                                      "deltaX": 0, "deltaY": -120 if up else 120})
                time.sleep(0.016)
        time.sleep(0.1)

    def choose_view(self, value: str) -> None:
        """Pick Main, All (flat) or `thread:ID` from the bar's selector, as a reader does."""
        cdp = self.cdp
        assert cdp is not None
        ok = cdp.eval(f"""(() => {{ const s = document.getElementById('thread-select');
            if (![...s.options].some((o) => o.value === {json.dumps(value)})) return false;
            if (s.value === {json.dumps(value)}) return true;
            s.value = {json.dumps(value)}; s.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()""")
        if not ok:
            raise CdpError(f"the thread selector offers no {value}")

    def busiest_thread(self) -> str:
        return self.channel.busiest_thread()

    def minimize(self, hidden: bool) -> str:
        cdp = self.cdp
        assert cdp is not None
        cdp.send("Browser.setWindowBounds", {"windowId": self.window_id,
                                             "bounds": {"windowState": "minimized" if hidden else "normal"}},
                 browser=True)
        time.sleep(1)
        return str(cdp.eval("document.visibilityState"))

    # --- measurement ------------------------------------------------------------------------------

    def snapshot(self, gc: bool = True) -> Json:
        cdp = self.cdp
        assert cdp is not None
        if gc:
            cdp.send("HeapProfiler.collectGarbage")
            time.sleep(0.3)
        m = cdp.metrics()
        out: Json = {k: m.get(k) for k in SNAPSHOT_KEYS}
        out["dom_elements"] = cdp.eval("document.getElementsByTagName('*').length")
        out["rows"] = cdp.eval("document.querySelectorAll('#discord-log > li').length")
        return out

    def measure(self, name: str, action: Callable[[], Json | None], results: list[Json],
                profile_cpu: bool = False) -> Json:
        """Run `action` and record what it cost: Chromium's processes, the server, the renderer's own
        accounting, long tasks and the bridge requests it caused; optionally a sampled profile."""
        cdp = self.cdp if self.browser is not None else None
        meter = CpuMeter(self.browser.pid) if self.browser is not None else None
        longtasks = "[__energy.longtasks, __energy.longtaskMs]"
        lt0: list[object] = [0, 0]
        m0: dict[str, float] = {}
        if cdp is not None:
            cdp.drain()
            if profile_cpu:
                cdp.send("Profiler.enable")
                cdp.send("Profiler.setSamplingInterval", {"interval": 500})
                cdp.send("Profiler.start")
            lt0, m0 = items(cdp.eval(longtasks)), cdp.metrics()
        s0 = self.server_cpu()
        bridge0 = dict(self.bridge_counts)
        wall0 = time.monotonic()
        if meter:
            meter.start()
        extra = action() or {}
        chrome = meter.stop() if meter else {"total": 0.0}
        wall = time.monotonic() - wall0
        s1 = self.server_cpu()
        lt1, m1 = (items(cdp.eval(longtasks)), cdp.metrics()) if cdp is not None else (lt0, m0)
        record: Json = {
            "segment": name, "wall_s": round(wall, 2), "chrome_cpu_s": chrome,
            "server_cpu_s": round(s1 - s0, 3),
            "renderer": {k: round(m1.get(k, 0) - m0.get(k, 0), 4) for k in METRIC_KEYS},
            "longtasks": [num(lt1[0]) - num(lt0[0]), round(num(lt1[1]) - num(lt0[1]), 1)],
            "bridge_requests": {k: v - bridge0.get(k, 0) for k, v in self.bridge_counts.items()
                                if v - bridge0.get(k, 0)},
            **extra,
        }
        if cdp is not None and profile_cpu:
            record["cpu_profile"] = summarize_profile(obj(cdp.send("Profiler.stop", timeout=300)["profile"]))
            cdp.send("Profiler.disable")
        results.append(record)
        print(f"  [{self.profile}] {name}: chrome {chrome.get('total', 0):.2f}s cpu "
              f"(renderer {chrome.get('renderer', 0):.2f}s), server {s1 - s0:.2f}s, "
              f"wall {wall:.0f}s", file=sys.stderr, flush=True)
        return record

    def census_report(self) -> Json:
        cdp = self.cdp
        assert cdp is not None
        return obj(cdp.eval("""(() => { const C = window.__census; if (!C) return {};
            const round = (t) => Object.fromEntries(Object.entries(t).map(([k, v]) =>
              [k, Object.fromEntries(Object.entries(v).map(([f, n]) => [f, Math.round(n * 10) / 10]))]));
            return {timers: round(C.timers), raf: round(C.raf), observers: round(C.observers),
                    fetches: C.fetches, listeners: C.listeners, animations: C.animations}; })()"""))

    def census_reset(self) -> None:
        assert self.cdp is not None
        self.cdp.eval("""(() => { const C = window.__census; if (!C) return;
            for (const k of ['timers', 'raf', 'observers', 'fetches', 'listeners', 'animations'])
              for (const key of Object.keys(C[k])) delete C[k][key]; })()""")

    def idle(self, seconds: float, census_every: float = 5.0) -> None:
        """Sit still. In a census pass, sample the running CSS animations as it goes."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            time.sleep(min(census_every if self.census else 30.0, max(0.0, deadline - time.monotonic())))
            if self.census and self.cdp is not None:
                self.cdp.eval("window.__census && __census.sampleAnimations()")
            if self.cdp is not None:
                self.cdp.drain()

    def close(self) -> None:
        if self.cdp:
            self.cdp.close()
        for proc in (self.browser, self.server):
            if proc and proc.poll() is None:
                proc.send_signal(signal.SIGTERM)
                try:
                    proc.wait(10)
                except subprocess.TimeoutExpired:
                    proc.kill()
        if getattr(self, "httpd", None):
            self.httpd.shutdown()
        shutil.rmtree(self.workdir, ignore_errors=True)


def summarize_profile(profile: Json, top: int = 30) -> Json:
    """Self and inclusive milliseconds per function from a sampled V8 profile."""
    nodes = {int(num(obj(n)["id"])): obj(n) for n in items(profile["nodes"])}
    parent: dict[int, int] = {}
    for ident, n in nodes.items():
        for child in items(n.get("children", [])):
            parent[int(num(child))] = ident

    def key(node: Json) -> str:
        frame = obj(node["callFrame"])
        name = text(frame.get("functionName") or "(anonymous)")
        url = text(frame.get("url", "")).rsplit("/", 1)[-1].split("?")[0]
        return f"{name} {url}:{int(num(frame.get('lineNumber', -1))) + 1}" if url else name

    self_ms: dict[str, float] = {}
    incl_ms: dict[str, float] = {}
    samples = [int(num(sample)) for sample in items(profile.get("samples", []))]
    deltas = [num(delta) for delta in items(profile.get("timeDeltas", []))]
    for i, sid in enumerate(samples):
        dt = (deltas[i + 1] if i + 1 < len(deltas) else 0) / 1000.0
        node = nodes[sid]
        k = key(node)
        self_ms[k] = self_ms.get(k, 0.0) + dt
        seen = set()
        cur: int | None = sid
        while cur is not None:
            kk = key(nodes[cur])
            if kk not in seen:
                incl_ms[kk] = incl_ms.get(kk, 0.0) + dt
                seen.add(kk)
            cur = parent.get(cur)
    idle = {"(idle)", "(program)", "(garbage collector)", "(root)"}
    busy = sum(v for k, v in self_ms.items() if k != "(idle)")
    return {
        "busy_ms": round(busy, 1),
        "gc_ms": round(self_ms.get("(garbage collector)", 0.0), 1),
        "program_ms": round(self_ms.get("(program)", 0.0), 1),
        "self": [[k, round(v, 1)] for k, v in sorted(self_ms.items(), key=lambda kv: -kv[1])
                 if k not in idle][:top],
        "inclusive": [[k, round(v, 1)] for k, v in sorted(incl_ms.items(), key=lambda kv: -kv[1])
                      if k not in idle][:top],
    }


# =================================================================================================
# Scenarios.
# =================================================================================================

def scenario_a(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    rig.measure("server-idle-no-page", lambda: rig.idle(opts.server_seconds), out)
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    out.append({"segment": "snapshot-loaded", **rig.snapshot()})
    if rig.census:
        rig.census_reset()
    rig.measure("a-idle-visible", lambda: rig.idle(opts.idle_seconds), out, opts.cpu_profile)
    if rig.census:
        out.append({"segment": "census-a-idle-visible", "census": rig.census_report()})


def scenario_b(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    """Hidden: two back-to-back idle windows (Chromium's intensive timer throttling starts five minutes
    after a page is hidden), then live traffic while still hidden, then the return."""
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    if rig.census:
        rig.census_reset()
    visibility = rig.minimize(True)

    def hidden_idle() -> Json:
        rig.idle(opts.hidden_seconds)
        return {"visibility": visibility}

    def hidden_live() -> Json:
        sent = live_traffic(rig, opts.live_seconds, opts.live_every, 0.0, None)
        return {**sent, "visibility": str(rig.cdp.eval("document.visibilityState")) if rig.cdp else "?"}

    def back() -> Json:
        shown: Json = {"visibility": rig.minimize(False)}
        time.sleep(15)
        return shown

    for part in ("first", "second"):
        rig.measure(f"b-idle-hidden-{part}", hidden_idle, out, opts.cpu_profile)
        if rig.census:
            out.append({"segment": f"census-b-idle-hidden-{part}", "census": rig.census_report()})
            rig.census_reset()
    rig.measure("b-live-hidden", hidden_live, out)
    if rig.census:
        out.append({"segment": "census-b-live-hidden", "census": rig.census_report()})
        rig.census_reset()
    rig.measure("b-return-visible-15s", back, out)
    if rig.census:
        out.append({"segment": "census-b-return", "census": rig.census_report()})


def live_traffic(rig: Rig, seconds: float, every: float, thread_share: float, thread_id: str | None) -> Json:
    """One message every `every` seconds, pushed through the server's live route."""
    rng = random.Random(77)
    sent = 0
    errors = 0
    deadline = time.monotonic() + seconds
    next_at = time.monotonic()
    while time.monotonic() < deadline:
        time.sleep(max(0.0, next_at - time.monotonic()))
        next_at += every
        target = thread_id if thread_id and rng.random() < thread_share else (
            rig.busiest_thread() if rng.random() < 0.3 else None)
        message = rig.channel.append(target)
        try:
            rig.api("POST", "/api/v1/live/events",
                    {"event_id": f"ev-{message['id']}", "historical": False, "kind": "create",
                     "message": message}, token=INGEST_TOKEN)
            sent += 1
        except (urllib.error.URLError, OSError):
            errors += 1
        if rig.cdp is not None:
            rig.cdp.drain()
    return {"sent": sent, "send_errors": errors}


def scenario_c_e_g(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    out.append({"segment": "snapshot-loaded", **rig.snapshot()})
    thread = rig.busiest_thread()
    for view, label in (("main", "main"), ("flat", "all"), (f"thread:{thread}", "thread")):
        rig.choose_view(view)
        time.sleep(3)
        if rig.census:
            rig.census_reset()
        rig.measure(f"c-live-{label}", lambda: live_traffic(
            rig, opts.live_seconds, opts.live_every, 0.6 if label == "thread" else 0.0,
            thread if label == "thread" else None), out, opts.cpu_profile)
        if rig.census:
            out.append({"segment": f"census-c-live-{label}", "census": rig.census_report()})
    out.append({"segment": "snapshot-after-c", **rig.snapshot()})
    for round_ in (1, 2):
        rig.measure(f"e-switches-round{round_}", lambda: switches(rig, opts), out,
                    opts.cpu_profile and round_ == 1)
        out.append({"segment": f"snapshot-after-e{round_}", **rig.snapshot()})


def switches(rig: Rig, opts: argparse.Namespace) -> Json:
    """All -> Main -> thread x N, then the read mode cycled, then the row menu opened and closed."""
    cdp = rig.cdp
    assert cdp is not None
    thread = rig.busiest_thread()
    steps = 0
    rows_ready = "document.querySelectorAll('#discord-log > li').length > 0"
    for _ in range(opts.switch_rounds):
        for view in ("flat", "main", f"thread:{thread}"):
            rig.choose_view(view)
            cdp.wait_for(rows_ready, 15)
            time.sleep(opts.step_pause)
            steps += 1
    rig.choose_view("flat")
    time.sleep(0.5)
    modes = []
    for _ in range(9):
        rig.tap("#todo-filter")
        time.sleep(opts.step_pause)
        modes.append(cdp.eval("document.getElementById('todo-filter').dataset.readMode || "
                              "document.getElementById('todo-filter').getAttribute('aria-pressed')"))
        steps += 1
    # Back to the mode the reader started in.
    for _ in range(9):
        if cdp.eval("(document.getElementById('todo-filter').getAttribute('aria-pressed') || 'false') === 'false'"):
            break
        rig.tap("#todo-filter")
        time.sleep(opts.step_pause)
    menus = 0
    menu_misses = 0
    for i in range(opts.menu_rounds):
        has = cdp.eval(f"""(() => {{ const b = [...document.querySelectorAll('#discord-log > li .row-more-button')]
            .filter((n) => n.getClientRects().length); if (!b.length) return false;
            const n = b[{i} % b.length]; n.id = 'energy-menu-target'; return true; }})()""")
        if not has:
            break
        try:
            rig.tap("#energy-menu-target")
            time.sleep(opts.step_pause / 2)
            opened = cdp.eval("(() => { const b = document.getElementById('energy-menu-target');"
                              " return !!b && b.getAttribute('aria-expanded') === 'true'; })()")
            # Closed with Escape: a second tap where the button was can land on the open menu's
            # first item instead, which on a phone is one of the mark-read actions.
            for kind in ("keyDown", "keyUp"):
                cdp.send("Input.dispatchKeyEvent", {"type": kind, "key": "Escape", "code": "Escape",
                                                    "windowsVirtualKeyCode": 27})
            time.sleep(opts.step_pause / 2)
            menus += 1 if opened else 0
        except CdpError:
            # The row was redrawn under the tap (a read landed): the menu went with it.
            menu_misses += 1
        cdp.eval("document.getElementById('energy-menu-target') && document.getElementById('energy-menu-target').removeAttribute('id')")
    return {"view_steps": steps, "read_modes_seen": sorted({str(m) for m in modes}), "menu_toggles": menus,
            "menu_misses": menu_misses}


def scenario_d(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    rig.start_browser()
    rig.open_channel()
    rig.choose_view("flat")
    time.sleep(opts.settle)
    cdp = rig.cdp
    assert cdp is not None

    def scroll_through() -> Json:
        rows_js = "document.querySelectorAll('#discord-log > li').length"
        pages = 0
        gestures = 0
        for _ in range(opts.scroll_max_gestures):
            # Never drag past the top: on a phone that is the pull-to-refresh gesture, which reloads.
            top = cdp.eval_num("document.getElementById('scroll-area').scrollTop")
            if top >= 40:
                rig.scroll(True, min(450.0, top - 10))
                gestures += 1
                continue
            before = cdp.eval(rows_js)
            try:  # the page pages older history in by itself as the top comes into view
                cdp.wait_for(f"{rows_js} > {before}", 4)
                pages += 1
                continue
            except CdpError:
                pass
            if not cdp.eval("""(() => { const b = document.getElementById('load-older');
                    return !!(b && !b.hidden && b.getClientRects().length && !b.disabled); })()"""):
                break
            rig.tap("#load-older")
            try:
                cdp.wait_for(f"{rows_js} > {before}", 15)
                pages += 1
            except CdpError:
                break
        rows = cdp.eval(rows_js)
        down = 0
        for _ in range(opts.scroll_max_gestures):
            rig.scroll(False)
            down += 1
            if cdp.eval("(() => { const a = document.getElementById('scroll-area');"
                        " return a.scrollTop + a.clientHeight >= a.scrollHeight - 40; })()"):
                break
        return {"older_pages": pages, "gestures_up": gestures, "gestures_down": down, "rows": rows}

    rig.measure("d-scroll-all", scroll_through, out, opts.cpu_profile)
    out.append({"segment": "snapshot-after-d", **rig.snapshot()})


def scenario_f(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    rig.tap("#view-switch")
    cdp = rig.cdp
    assert cdp is not None
    cdp.wait_for("document.getElementById('pane-discord').hidden", 10)
    time.sleep(3)
    if rig.census:
        rig.census_reset()
    rig.measure("f-voice-pane-idle", lambda: rig.idle(opts.voice_seconds), out, opts.cpu_profile)
    if rig.census:
        out.append({"segment": "census-f-voice-pane", "census": rig.census_report()})


def scenario_p(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    """The cost of ONE background refresh, isolated: the page's own poll entry point, called
    --polls times back to back with nothing new to bring, in Main (one page of rows) and in All
    with the whole history walked back in. The cheapest way to compare many builds."""
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    cdp = rig.cdp
    assert cdp is not None
    poll = "loadDiscord({keepPosition: true, reason: 'poll'}).then(() => true)"

    def polls() -> Json:
        for _ in range(opts.polls):
            cdp.eval(poll, await_promise=True)
            time.sleep(0.3)
        return {"polls": opts.polls, "rows": cdp.eval("document.querySelectorAll('#discord-log > li').length")}

    rig.measure("p-poll-main", polls, out, opts.cpu_profile)
    rig.choose_view("flat")
    time.sleep(2)
    rows_js = "document.querySelectorAll('#discord-log > li').length"
    for _ in range(30):
        before = cdp.eval(rows_js)
        if not cdp.eval("""(() => { const b = document.getElementById('load-older');
                return !!(b && !b.hidden && b.getClientRects().length && !b.disabled); })()"""):
            break
        cdp.eval("document.getElementById('load-older').click()")
        try:
            cdp.wait_for(f"{rows_js} > {before}", 15)
        except CdpError:
            break
    cdp.eval("(() => { const a = document.getElementById('scroll-area'); a.scrollTop = a.scrollHeight; })()")
    time.sleep(2)
    rig.measure("p-poll-all-history", polls, out, opts.cpu_profile)


def scenario_e(rig: Rig, opts: argparse.Namespace, out: list[Json]) -> None:
    """Scenario e on its own, with no live traffic before it: for comparing many builds."""
    rig.start_browser()
    rig.open_channel()
    time.sleep(opts.settle)
    rig.measure("e-switches", lambda: switches(rig, opts), out, opts.cpu_profile)
    out.append({"segment": "snapshot-after-e", **rig.snapshot()})


SCENARIOS: dict[str, Callable[[Rig, argparse.Namespace, list[Json]], None]] = {
    "a": scenario_a, "b": scenario_b, "ceg": scenario_c_e_g, "d": scenario_d, "f": scenario_f, "p": scenario_p, "e": scenario_e,
}


# =================================================================================================
# Orchestration.
# =================================================================================================

def run_job(job: Json, opts: argparse.Namespace) -> Json:
    cpus = job.get("cpus")
    rig = Rig(server_bin=text(job["server_bin"]), profile=text(job["profile"]),
              cpus=text(cpus) if cpus else None, chrome=opts.chrome, census=bool(job.get("census", False)))
    out: list[Json] = []
    started = datetime.now(timezone.utc).isoformat()
    error = None
    try:
        rig.start_server()
        SCENARIOS[text(job["scenario"])](rig, opts, out)
    except Exception as exc:  # noqa: BLE001 - a failed job is reported, not fatal to the run
        error = f"{type(exc).__name__}: {exc}"
        print(f"  job {job['name']} FAILED: {error}", file=sys.stderr, flush=True)
    finally:
        exceptions = list(rig.cdp.exceptions) if rig.cdp else []
        rig.close()
    return {**job, "started": started, "error": error, "segments": out, "page_exceptions": exceptions[:20],
            "chrome_version": opts.chrome_version}


def find_chrome() -> str:
    base = Path.home() / ".cache" / "ms-playwright"
    found = sorted(base.glob("chromium-*/chrome-linux64/chrome"))
    if not found:
        raise SystemExit("no Chromium found; pass --chrome PATH or `python3 -m playwright install chromium`")
    return str(found[-1])


def cpu_blocks(per_job: int, count: int, first: int | None) -> list[str | None]:
    if per_job <= 0:
        return [None] * count
    cpus = sorted(os.sched_getaffinity(0))
    if first is not None:
        cpus = [c for c in cpus if c >= first]
    blocks = [cpus[i:i + per_job] for i in range(0, len(cpus) - per_job + 1, per_job)]
    return [",".join(map(str, blocks[i % len(blocks)])) for i in range(count)]


def plan(opts: argparse.Namespace) -> list[Json]:
    jobs: list[Json] = []
    builds = dict(b.split("=", 1) for b in opts.build)
    for repeat in range(opts.repeats):
        for scenario in opts.scenarios:
            for profile in opts.profiles:
                for label, path in builds.items():
                    jobs.append({"name": f"{label}-{profile}-{scenario}-r{repeat}", "build": label,
                                 "server_bin": path, "profile": profile, "scenario": scenario,
                                 "repeat": repeat, "kind": "measure"})
    for label, path in builds.items():
        for profile in opts.profiles:
            if opts.census:
                for scenario in [s for s in ("a", "b", "ceg", "f") if s in opts.scenarios]:
                    jobs.append({"name": f"{label}-{profile}-{scenario}-census", "build": label,
                                 "server_bin": path, "profile": profile, "scenario": scenario,
                                 "repeat": 0, "kind": "census", "census": True})
            if opts.cpu_profile_pass:
                for scenario in [s for s in ("a", "ceg", "d", "p") if s in opts.scenarios]:
                    jobs.append({"name": f"{label}-{profile}-{scenario}-profile", "build": label,
                                 "server_bin": path, "profile": profile, "scenario": scenario,
                                 "repeat": 0, "kind": "profile", "cpu_profile": True})
    return jobs


def worker_main(opts: argparse.Namespace) -> int:
    # A stopped run must not leave its server and browser behind: turn SIGTERM and SIGHUP into an
    # exit, so the job's own teardown runs.
    def stop(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGHUP, stop)
    job = json.loads(opts.job)
    if job.get("cpu_profile"):
        opts.cpu_profile = True
    result = run_job(job, opts)
    Path(opts.result).write_text(json.dumps(result, indent=1))
    return 0 if result["error"] is None else 1


def orchestrate(opts: argparse.Namespace) -> int:
    out = Path(opts.out)
    out.mkdir(parents=True, exist_ok=True)
    jobs = plan(opts)
    blocks = cpu_blocks(opts.cores_per_job, opts.parallel, opts.first_cpu)
    print(f"{len(jobs)} jobs, {opts.parallel} at a time; results in {out}", file=sys.stderr)
    free = list(range(opts.parallel))
    lock = threading.Lock()

    def launch(job: Json) -> int:
        target = out / f"{job['name']}.json"
        if target.exists() and not opts.force:
            return 0
        with lock:
            slot = free.pop()
        try:
            job = {**job, "cpus": blocks[slot]}
            argv = [sys.executable, __file__, "--worker", "--job", json.dumps(job), "--result", str(target),
                    "--chrome", opts.chrome, *passthrough(opts)]
            print(f"start {job['name']} on cpus {job['cpus']}", file=sys.stderr, flush=True)
            with open(out / f"{job['name']}.log", "w") as log:
                code = subprocess.call(argv, stdout=log, stderr=subprocess.STDOUT)
            print(f"done  {job['name']} exit {code}", file=sys.stderr, flush=True)
            return code
        finally:
            with lock:
                free.append(slot)

    with concurrent.futures.ThreadPoolExecutor(opts.parallel) as pool:
        codes = list(pool.map(launch, jobs))
    report(out)
    if opts.compare:
        print(compare(out, opts.compare[0], opts.compare[1]))
    return 0 if all(c == 0 for c in codes) else 1


TUNABLES = ("polls", "idle_seconds", "hidden_seconds", "live_seconds", "live_every", "voice_seconds", "settle",
            "server_seconds", "switch_rounds", "menu_rounds", "step_pause", "scroll_max_gestures")


def passthrough(opts: argparse.Namespace) -> list[str]:
    args: list[str] = []
    for name in TUNABLES:
        args += [f"--{name.replace('_', '-')}", str(getattr(opts, name))]
    return args


# =================================================================================================
# Reporting.
# =================================================================================================

def spread(values: list[float]) -> str:
    if not values:
        return "-"
    med = statistics.median(values)
    if len(values) == 1:
        return f"{med:.3g}"
    return f"{med:.3g} [{min(values):.3g}-{max(values):.3g}]"


def report(out: Path) -> None:
    results = [json.loads(p.read_text()) for p in sorted(out.glob("*.json")) if p.name != "summary.json"]
    measured = [r for r in results if r.get("kind") == "measure"]
    table: dict[tuple[str, str, str], dict[str, list[float]]] = {}
    for r in measured:
        for seg in r["segments"]:
            if "chrome_cpu_s" not in seg and not seg["segment"].startswith("snapshot"):
                continue
            key = (seg["segment"], r["profile"], r["build"])
            row = table.setdefault(key, {})
            if "chrome_cpu_s" in seg:
                wall = max(seg["wall_s"], 1e-9)
                vals = {
                    "chrome_cpu_s": seg["chrome_cpu_s"].get("total", 0),
                    "renderer_cpu_s": seg["chrome_cpu_s"].get("renderer", 0),
                    "gpu_cpu_s": seg["chrome_cpu_s"].get("gpu-process", 0),
                    "browser_cpu_s": seg["chrome_cpu_s"].get("browser", 0),
                    "chrome_cpu_pct": 100 * seg["chrome_cpu_s"].get("total", 0) / wall,
                    "server_cpu_s": seg["server_cpu_s"],
                    "task_s": seg["renderer"]["TaskDuration"],
                    "script_s": seg["renderer"]["ScriptDuration"],
                    "layout_s": seg["renderer"]["LayoutDuration"],
                    "style_s": seg["renderer"]["RecalcStyleDuration"],
                    "layouts": seg["renderer"]["LayoutCount"],
                    "styles": seg["renderer"]["RecalcStyleCount"],
                    "longtasks": seg["longtasks"][0],
                    "wall_s": seg["wall_s"],
                }
            else:
                vals = {k: float(seg.get(k) or 0) for k in (*SNAPSHOT_KEYS, "dom_elements", "rows")}
            for k, v in vals.items():
                row.setdefault(k, []).append(float(v))
    summary = {f"{s}|{p}|{b}": {k: {"median": statistics.median(v), "min": min(v), "max": max(v), "n": len(v)}
                                for k, v in row.items()} for (s, p, b), row in table.items()}
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    lines = ["| segment | profile | build | chrome CPU s | % of a core | renderer s | task s | script s | "
             "layout s | style s | layouts | longtasks | server CPU s | n |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    snap = ["| checkpoint | profile | build | JS heap MB | nodes | DOM elements | listeners | rows | n |",
            "|---|---|---|---|---|---|---|---|---|"]
    for (s, p, b), row in sorted(table.items()):
        if "chrome_cpu_s" in row:
            lines.append(f"| {s} | {p} | {b} | {spread(row['chrome_cpu_s'])} | {spread(row['chrome_cpu_pct'])} | "
                         f"{spread(row['renderer_cpu_s'])} | {spread(row['task_s'])} | {spread(row['script_s'])} | "
                         f"{spread(row['layout_s'])} | {spread(row['style_s'])} | {spread(row['layouts'])} | "
                         f"{spread(row['longtasks'])} | {spread(row['server_cpu_s'])} | {len(row['chrome_cpu_s'])} |")
        else:
            snap.append(f"| {s} | {p} | {b} | {spread([v / 1e6 for v in row['JSHeapUsedSize']])} | "
                        f"{spread(row['Nodes'])} | {spread(row['dom_elements'])} | {spread(row['JSEventListeners'])} | "
                        f"{spread(row['rows'])} | {len(row['Nodes'])} |")
    failed = [f"{r['name']}: {r['error']}" for r in results if r.get("error")]
    text = "\n".join(lines) + "\n\n" + "\n".join(snap) + "\n"
    if failed:
        text += "\nFailed jobs:\n" + "\n".join(f"- {f}" for f in failed) + "\n"
    (out / "summary.md").write_text(text)
    print(text)


def compare(out: Path, before: str, after: str) -> str:
    """Two builds of one run side by side, from its summary.json: each measure's median with its
    (min-max) spread, and the change in the median. Written to compare.md as well."""
    summary = obj(json.loads((out / "summary.json").read_text()))

    def stat(segment: str, profile: str, build: str, metric: str, scale: float = 1.0) -> tuple[float, str] | None:
        row = summary.get(f"{segment}|{profile}|{build}")
        if not isinstance(row, dict) or metric not in row:
            return None
        cell = obj(row[metric])
        med, low, high = num(cell["median"]) * scale, num(cell["min"]) * scale, num(cell["max"]) * scale
        said = f"{med:.3g}" if num(cell["n"]) < 2 else f"{med:.3g} ({low:.3g}-{high:.3g})"
        return med, said

    def pair(segment: str, profile: str, metric: str, scale: float = 1.0) -> tuple[str, str, str]:
        a, b = stat(segment, profile, before, metric, scale), stat(segment, profile, after, metric, scale)
        if a is None or b is None:
            return (a[1] if a else "-", b[1] if b else "-", "")
        change = f"{100 * (b[0] - a[0]) / a[0]:+.0f}%" if a[0] else ""
        return a[1], b[1], change

    keys = sorted({tuple(text(k).split("|")[:2]) for k in summary})
    lines = [f"| segment | profile | Chromium CPU s, {before} | {after} | change | renderer CPU s, {before} | "
             f"{after} | change | layouts {before} → {after} | long tasks {before} → {after} | server CPU s "
             f"{before} → {after} |", "|---|---|---|---|---|---|---|---|---|---|---|"]
    snaps = [f"| checkpoint | profile | JS heap MB, {before} → {after} | nodes | listeners | rows |",
             "|---|---|---|---|---|---|"]
    for segment, profile in keys:
        if stat(segment, profile, before, "chrome_cpu_s") or stat(segment, profile, after, "chrome_cpu_s"):
            chrome, renderer = pair(segment, profile, "chrome_cpu_s"), pair(segment, profile, "renderer_cpu_s")
            lay, longs, server = (pair(segment, profile, m) for m in ("layouts", "longtasks", "server_cpu_s"))
            lines.append(f"| {segment} | {profile} | {' | '.join(chrome)} | {' | '.join(renderer)} | "
                         f"{lay[0]} → {lay[1]} | {longs[0]} → {longs[1]} | {server[0]} → {server[1]} |")
        elif stat(segment, profile, before, "Nodes") or stat(segment, profile, after, "Nodes"):
            heap = pair(segment, profile, "JSHeapUsedSize", 1e-6)
            nodes, listeners, rows = (pair(segment, profile, m) for m in ("Nodes", "JSEventListeners", "rows"))
            snaps.append(f"| {segment} | {profile} | {heap[0]} → {heap[1]} | {nodes[0]} → {nodes[1]} | "
                         f"{listeners[0]} → {listeners[1]} | {rows[0]} → {rows[1]} |")
    said = "\n".join(lines) + "\n\n" + "\n".join(snaps) + "\n"
    (out / "compare.md").write_text(said)
    return said


# =================================================================================================

def arguments(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--build", action="append", default=[], metavar="LABEL=PATH",
                   help="a vibe-talk server binary to measure, under a label; repeat for each build")
    p.add_argument("--out", default=None, help="directory for per-job JSON, logs and summary.{md,json}")
    p.add_argument("--report-only", metavar="DIR", help="only re-aggregate the results already in DIR")
    p.add_argument("--compare", metavar="BEFORE,AFTER", type=lambda s: s.split(","), default=None,
                   help="after the tables, put two of the run's build labels side by side, with the change "
                        "in each median (also written to compare.md); with --report-only or a run")
    p.add_argument("--scenarios", default="a,b,ceg,d,f", type=lambda s: s.split(","),
                   help="jobs to run: a, b, ceg (c, e and g in one page), d, f, and p, the per-refresh "
                        "micro-benchmark, and e alone, both for comparing many builds (default: a,b,ceg,d,f)")
    p.add_argument("--profiles", default="mobile,desktop", type=lambda s: s.split(","),
                   help="device profiles: mobile (412x915 touch DPR 2.625), desktop (1280x800) (default: both)")
    p.add_argument("--repeats", type=int, default=3, help="measured runs per build, profile and job (default 3)")
    p.add_argument("--parallel", type=int, default=4, help="jobs at once, each its own server and browser (default 4)")
    p.add_argument("--cores-per-job", type=int, default=4,
                   help="CPUs each job's browser and server are pinned to with taskset; 0 = no pinning (default 4)")
    p.add_argument("--first-cpu", type=int, default=None, help="lowest CPU number to pin to (default: any)")
    p.add_argument("--census", action="store_true", help="add one instrumented census job per build/profile")
    p.add_argument("--cpu-profile-pass", action="store_true",
                   help="add one sampled-V8-profile job per build/profile for a, ceg and d")
    p.add_argument("--chrome", default=None, help="Chromium executable (default: Playwright's newest bundled)")
    p.add_argument("--force", action="store_true", help="re-run jobs whose result file already exists")
    p.add_argument("--quick", action="store_true", help="shorten every segment, for a smoke run of the harness")
    p.add_argument("--polls", type=int, default=20,
                   help="job p's background refreshes per segment, called through the page's own poll entry "
                        "point (default 20)")
    p.add_argument("--idle-seconds", type=float, default=300, help="scenario a's idle window, seconds (default 300)")
    p.add_argument("--hidden-seconds", type=float, default=300, help="scenario b's hidden window, seconds (default 300)")
    p.add_argument("--live-seconds", type=float, default=180, help="scenario c's window per view, seconds (default 180)")
    p.add_argument("--live-every", type=float, default=10, help="scenario c's seconds between messages (default 10)")
    p.add_argument("--voice-seconds", type=float, default=180, help="scenario f's window, seconds (default 180)")
    p.add_argument("--server-seconds", type=float, default=60,
                   help="job a's server-only window before any page opens, seconds (default 60)")
    p.add_argument("--settle", type=float, default=15, help="seconds after load before measuring (default 15)")
    p.add_argument("--switch-rounds", type=int, default=20, help="scenario e's All->Main->thread rounds (default 20)")
    p.add_argument("--menu-rounds", type=int, default=20, help="scenario e's row-menu open/close pairs (default 20)")
    p.add_argument("--step-pause", type=float, default=0.6, help="scenario e's pause after each action, s (default 0.6)")
    p.add_argument("--scroll-max-gestures", type=int, default=250,
                   help="scenario d's cap on scroll gestures each way (default 250)")
    p.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    p.add_argument("--job", help=argparse.SUPPRESS)
    p.add_argument("--result", help=argparse.SUPPRESS)
    p.add_argument("--cpu-profile", action="store_true", help=argparse.SUPPRESS)
    opts = p.parse_args(argv)
    if opts.quick:
        for name, value in (("idle_seconds", 20), ("hidden_seconds", 20), ("live_seconds", 20),
                            ("voice_seconds", 10), ("server_seconds", 5), ("settle", 3),
                            ("switch_rounds", 2), ("menu_rounds", 3)):
            setattr(opts, name, value)
    return opts


def main(argv: list[str]) -> int:
    opts = arguments(argv)
    if opts.compare is not None and len(opts.compare) != 2:
        raise SystemExit("--compare takes two build labels, BEFORE,AFTER")
    if opts.report_only:
        report(Path(opts.report_only))
        if opts.compare:
            print(compare(Path(opts.report_only), opts.compare[0], opts.compare[1]))
        return 0
    opts.chrome = opts.chrome or find_chrome()
    try:
        opts.chrome_version = subprocess.run([opts.chrome, "--version"], capture_output=True, text=True,
                                             timeout=30).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        opts.chrome_version = "unknown"
    if opts.worker:
        return worker_main(opts)
    if not opts.build or not opts.out:
        raise SystemExit("--build LABEL=PATH (at least one) and --out DIR are required")
    return orchestrate(opts)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
