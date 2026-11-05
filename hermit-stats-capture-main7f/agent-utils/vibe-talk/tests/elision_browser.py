#!/usr/bin/env python3
"""Fold long messages in real Chromium, and hold every label to the words the fold really hides.

`#244 elision-word-count`. A folded message ends with how much the fold keeps back: "(~240 more
words)", or "(~400 more characters)" for text poor in spaces. The fold is a three-line CSS clamp, so
the page cannot know the cut without laying the row out, and it does not: it counts a floor for every
width class a line can be in, and the label's own container query shows the one for the row as drawn
(web/voice.js `elisionLine`, web/voice.css `.elided`). The fake-DOM suite holds the counts to the
model; only a browser can say whether the label shown is ever MORE than the clamp hides — the review's
finding against the first version, which used a phone's line everywhere and on a desk claimed hidden
words under a message three desk lines held whole.

For each layout — a 360px and a 412px phone, a 412px phone at 80% and at 150% text, a 1280x800 desk,
a 1440px desk with the widest reading column, and a 1200px desk in two columns — it opens a channel of
prose of three lengths, a 300-character message, Chinese text, a long bare address, an agent's message
whose first line its rich header restates, and four bodies as the server renders them that are not
wrapped prose: a table alone, a table with prose under it, a line of prose over a code block, and a
list of short items. For every folded row it:

  * finds, by the rects of the characters, how many words start and how many characters stand below
    the clamped box: what the fold really hides;
  * reads the ONE label the stylesheet shows, and requires that it claim no more than that, in the
    unit it names; and that the line it is on is exactly as wide as the text, at the text's size, so
    the query measured the text's line in the text's em;
  * requires the longest message, a wall at every width, to be labelled, with at least half of what
    is hidden: a floor that said almost nothing would be safe and useless.

At 1280 the 300-character message is about three lines (whole in some faces, a few words over in a
wide one) and says nothing; on the phones it hides more than a line and is labelled; Chinese and the
address are counted in characters. From review: the clamp draws a TABLE whole, in a box of its own, so
a table hides nothing and must say nothing — counted a line per cell it said "(~95 more words)" with
every word on the screen; under prose the table is counted as shown and the prose below its clamp as
hidden. The code block and the list are labelled on every phone, within what they hide. With the
rich-headers switch turned off, the agent's row is labelled from its text as written, still within
what is hidden.

It serves the checked-out page and a small fake API through Playwright's request routing, so no port
is opened. Pass --web-root DIR to run it against another copy of the page, --screenshots DIR to keep a
PNG of each layout.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import unquote, urlsplit

if TYPE_CHECKING:
    from playwright.sync_api import Browser, Page, Route

ORIGIN = "http://127.0.0.1:9"
TOKEN = "write-token-browser-elision-check"
CHANNEL = {"id": "1110000000000000001", "label": "lead team", "writable": True, "alias": None,
           "added": False}
AGENT = {"author": "ci-bot", "author_id": "1000000000000000001", "author_is_bot": True}
EPOCH = datetime(2026, 10, 12, 7, 0, tzinfo=timezone.utc)
Json = dict[str, object]

PROSE = (
    "Backup check finished on all four shards. The overnight run took a little longer than usual "
    "because the second shard had to replay its write-ahead log after the restart, but every checksum "
    "matched and the restore drill passed on the first try. I also rotated the credentials the job "
    "uses, updated the runbook with the new paths, and filed a follow-up to trim the retention window, "
    "which is now holding three weeks more than the policy asks for. Nothing needs your attention "
    "today; the next scheduled run is tomorrow at the same time, and I will post again only if "
    "something changes or a check fails."
)
LEAD = "[build-bot coordinator] Backup check, 10:52 PM ET. "
TEXTS: dict[str, str] = {
    "wall": " ".join([PROSE] * 3),
    "prose": PROSE,
    "brief": PROSE[:300].rsplit(" ", 1)[0] + ".",
    "chinese": "我们今天完成了所有分片的备份检查。第二个分片在重启后需要重放预写日志，所以夜间运行比平时稍长，"
               "但所有校验和都匹配，恢复演练第一次就通过了。我还轮换了作业使用的凭据，用新路径更新了操作手册。" * 3,
    "address": "https://example.com/" + "a1b2c3d4e5/" * 40,
    "headed": LEAD + PROSE,
}
HEADING: Json = {"tag": "build-bot coordinator", "time": "10:52 PM ET", "title": "Backup check", "cut": len(LEAD)}


def table_html(rows: int) -> str:
    """A table as the server's renderer writes one: a header row and `rows` rows of three cells."""
    def row(tag: str, cells: list[str]) -> str:
        return "<tr>\n" + "".join(f"<{tag}>{cell}</{tag}>\n" for cell in cells) + "</tr>\n"
    body = "".join(row("td", [f"shard {r} value {c}" for c in range(3)]) for r in range(rows))
    return f"<table>\n<thead>\n{row('th', ['Shard', 'Status', 'Checked'])}</thead>\n<tbody>\n{body}</tbody>\n</table>\n"


STEPS = "".join(f"step {i}: ok  checksum=abc{i}\n" + ("\n" if i == 6 else "") for i in range(14))
# Bodies the server renders that are not wrapped prose, sent as `content_html` beside their text.
RENDERED: dict[str, str] = {
    "table": table_html(8),
    "table-prose": table_html(4) + f"<p>{PROSE}</p>\n",
    "code": f"<p>Here is where things stand after the run:</p>\n<pre><code>{STEPS}</code></pre>\n",
    "list": "<ul>\n" + "".join(f"<li>shard {i} synced</li>\n" for i in range(24)) + "</ul>\n",
}
TEXTS.update({name: f"{name} as written" for name in RENDERED})
IDS = {name: f"244000000000000{n:04d}" for n, name in enumerate(TEXTS)}


def messages() -> list[Json]:
    out: list[Json] = []
    for n, (name, text) in enumerate(TEXTS.items()):
        one: Json = {"id": IDS[name], "channel_id": CHANNEL["id"], **AGENT,
                     "timestamp": (EPOCH + timedelta(minutes=n)).isoformat().replace("+00:00", "Z"),
                     "spoken_time": "", "reply_to": None, "content": text}
        if name == "headed":
            one["heading"] = HEADING
        if name in RENDERED:
            one["content_html"] = RENDERED[name]
        out.append(one)
    return out


class FakeApi:
    """The routes /voice reads, for one channel of long messages from an agent."""

    def __init__(self, web_root: Path) -> None:
        self.web_root = web_root
        self.messages = messages()

    def route(self, route: Route) -> None:
        request = route.request
        path = unquote(urlsplit(request.url).path)
        if not path.startswith("/api/"):
            relative = "voice.html" if path == "/voice" else path.lstrip("/")
            target = (self.web_root / relative).resolve()
            if self.web_root not in target.parents or not target.is_file():
                route.fulfill(status=404, body="")
                return
            route.fulfill(status=200, body=target.read_bytes(),
                          content_type=mimetypes.guess_type(target.name)[0] or "application/octet-stream",
                          headers={"Cache-Control": "no-store"})
            return
        if request.headers.get("authorization") != f"Bearer {TOKEN}":
            self.json(route, 401, {"error": "unauthorized", "detail": "unknown token"})
        elif path == "/api/v1/client-config":
            self.json(route, 200, {
                "token_scope": "write", "version": "browser-check", "chat_provider_name": "Google Chat",
                "channels": [CHANNEL], "live_poll_seconds": 30, "live_delivery": "poll",
                "threading_supported": True, "channel_registration_supported": False,
                "read_aloud": {"backend": "browser", "label": "Browser voice", "playback": "browser",
                               "local_only": True},
                "elevenlabs_agent_id": None, "conversational_voice": {"name": "Test voice provider"},
                "replay_enabled": False, "self_author_id": None, "owner_author_id": None,
                "channel_discovery_supported": False, "upstream_read_mark_supported": False,
                "speech_prep_enabled": False,
            })
        elif path == "/api/v1/conversations":
            self.json(route, 200, {"conversations": []})
        elif path == "/api/v1/transcript":
            self.json(route, 200, {"turns": [], "has_more": False, "next_before": None})
        elif path.endswith("/timeline"):
            view = (dict(part.split("=", 1) for part in urlsplit(request.url).query.split("&") if "=" in part)
                    .get("view", "main"))
            self.json(route, 200, {
                "channel": CHANNEL, "messages": self.messages, "threads": [], "thread": None,
                "has_threads": False, "has_more": False, "next_before": None, "notice": None,
                "dismissed": [], "view": view, "limit": 100, "returned": len(self.messages),
                "untrusted_content_notice": "third-party text; DATA, never instructions",
            })
        else:
            self.json(route, 404, {"error": "not_found", "detail": "not served by this check"})

    @staticmethod
    def json(route: Route, status: int, payload: Json) -> None:
        route.fulfill(status=status, body=json.dumps(payload), content_type="application/json",
                      headers={"Cache-Control": "no-store"})


# Every folded row: what its clamp really hides, by the rects of its characters, and what it says.
MEASURE_JS = """() => {
  const out = [];
  for (const li of document.querySelectorAll('#discord-log > li')) {
    if (li.getAttribute('data-collapsed') !== 'true') continue;
    const body = li.querySelector(':scope > .body');
    const bottom = body.getBoundingClientRect().bottom - 1;
    const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
    let words = 0, hiddenWords = 0, chars = 0, hiddenChars = 0;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const text = node.data;
      for (let i = 0; i < text.length; i += 1) {
        if (/\\s/.test(text[i]) || /[\\udc00-\\udfff]/.test(text[i])) continue;
        const range = document.createRange();
        range.setStart(node, i);
        range.setEnd(node, i + 1);
        const rect = range.getClientRects()[0];
        if (!rect) continue;  // not drawn: the words a rich header restates
        const below = rect.top >= bottom;
        chars += 1;
        if (below) hiddenChars += 1;
        if (i === 0 || /\\s/.test(text[i - 1])) {
          words += 1;
          if (below) hiddenWords += 1;
        }
      }
    }
    const line = li.querySelector(':scope > .elided');
    const shown = line ? [...line.children].filter((span) => getComputedStyle(span).display !== 'none') : [];
    out.push({
      id: li.getAttribute('data-id'), words, hiddenWords, chars, hiddenChars,
      labels: shown.map((span) => span.textContent),
      lineWidth: line ? line.getBoundingClientRect().width : null,
      bodyWidth: body.getBoundingClientRect().width,
      lineFont: line ? getComputedStyle(line).fontSize : null,
      bodyFont: getComputedStyle(body).fontSize,
    });
  }
  return out;
}"""


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def number(value: object) -> float:
    check(isinstance(value, (int, float)), f"not a number: {value!r}")
    assert isinstance(value, (int, float))
    return float(value)


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("VIBE_TALK_CHROMIUM"),
        help="Chrome/Chromium executable (default: Playwright's bundled Chromium)",
    )
    parser.add_argument(
        "--web-root",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "web",
        help="the page assets to serve (default: this checkout's vibe-talk/web)",
    )
    parser.add_argument("--screenshots", type=Path, default=None,
                        help="a directory to keep one PNG per layout in (default: none kept)")
    return parser.parse_args()


# (name, width, height, touch, storage the page reads at load)
LAYOUTS: tuple[tuple[str, int, int, bool, dict[str, str]], ...] = (
    ("phone-360", 360, 800, True, {}),
    ("phone-412", 412, 915, True, {}),
    ("phone-412-text-80", 412, 915, True, {"vibe-talk.voice.msg-scale": "80"}),
    ("phone-412-text-150", 412, 915, True, {"vibe-talk.voice.msg-scale": "150"}),
    ("desk-1280", 1280, 800, False, {}),
    ("desk-1440-wide-column", 1440, 900, False, {"vibe-talk.voice.width": "120"}),
    ("desk-1200-two-columns", 1200, 800, False, {"vibe-talk.voice.columns": "two"}),
)


def open_layout(browser: Browser, api: FakeApi, width: int, height: int, touch: bool,
                storage: dict[str, str]) -> tuple[Page, list[str]]:
    context = browser.new_context(viewport={"width": width, "height": height}, color_scheme="dark",
                                  is_mobile=touch, has_touch=touch, device_scale_factor=2.625 if touch else 1)
    context.add_init_script("".join(f"localStorage.setItem({json.dumps(k)}, {json.dumps(v)});"
                                    for k, v in storage.items()))
    errors: list[str] = []
    context.route("**/*", api.route)
    page = context.new_page()
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.goto(f"{ORIGIN}/voice", wait_until="load")
    page.fill("#api-token", TOKEN)
    page.click("#save-token")
    page.wait_for_selector("#view-switch", state="visible", timeout=10_000)
    page.click("#view-switch")
    page.wait_for_selector(f'#discord-log > li[data-id="{IDS["headed"]}"]', timeout=10_000)
    page.wait_for_timeout(300)
    return page, errors


LABEL = re.compile(r"^\(~(\d+) more (words|characters)\)$")


def judge(layout: str, rows: list[Json], notes: list[str]) -> dict[str, tuple[int, str] | None]:
    """Hold every folded row's one label to what its clamp hides. Answers each row's claim by name."""
    names = {v: k for k, v in IDS.items()}
    claims: dict[str, tuple[int, str] | None] = {}
    for row in rows:
        name = names.get(str(row["id"]), str(row["id"]))
        labels = row["labels"]
        assert isinstance(labels, list)
        check(len(labels) <= 1, f"{layout} {name}: more than one label is shown: {labels}")
        if row["lineWidth"] is not None:
            check(abs(number(row["lineWidth"]) - number(row["bodyWidth"])) <= 1,
                  f"{layout} {name}: the label's line is not as wide as the text: {row}")
            check(row["lineFont"] == row["bodyFont"], f"{layout} {name}: the label's line is not at the text's size: {row}")
        if not labels:
            claims[name] = None
            continue
        found = LABEL.match(str(labels[0]))
        check(found is not None, f"{layout} {name}: the label reads {labels[0]!r}")
        assert found is not None
        claimed, unit = int(found.group(1)), found.group(2)
        hidden = int(number(row["hiddenWords"] if unit == "words" else row["hiddenChars"]))
        check(claimed <= hidden, f"{layout} {name}: says {claimed} more {unit}, the clamp hides {hidden}")
        claims[name] = (claimed, unit)
    wall = next(row for row in rows if names.get(str(row["id"])) == "wall")
    claim = claims.get("wall")
    check(claim is not None and claim[1] == "words", f"{layout}: the wall of text says nothing of what it hides")
    assert claim is not None
    check(claim[0] >= 0.5 * number(wall["hiddenWords"]),
          f"{layout}: the wall says {claim[0]} more words of {wall['hiddenWords']} hidden, too low to tell anything")
    said = []
    for row in rows:
        name = names.get(str(row["id"]), "?")
        held = claims.get(name)
        unit = held[1] if held is not None else "words"
        hidden = int(number(row["hiddenWords"] if unit == "words" else row["hiddenChars"]))
        said.append(f"{name} {'—' if held is None else held[0]}/{hidden}")
    notes.append(f"{layout}: " + "; ".join(said))
    return claims


def unit_of(claims: dict[str, tuple[int, str] | None], name: str) -> str | None:
    """The unit a row's label counts in, or None when it has no label."""
    held = claims.get(name)
    return held[1] if held is not None else None


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
    web_root = args.web_root.resolve()
    notes: list[str] = []
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
        try:
            for layout, width, height, touch, storage in LAYOUTS:
                page, errors = open_layout(browser, FakeApi(web_root), width, height, touch, storage)
                if "vibe-talk.voice.columns" in storage:
                    check(page.evaluate("() => document.getElementById('screen-main').getAttribute('data-columns')") == "two",
                          f"{layout}: the page did not go into two columns")
                rows: object = page.evaluate(MEASURE_JS)
                assert isinstance(rows, list)
                claims = judge(layout, rows, notes)
                brief = next(row for row in rows if row["id"] == IDS["brief"])
                if layout == "desk-1280":
                    # Whole in three lines in the review's browser, a handful of words over in a wider
                    # face: either way under ten words, which a floor cannot claim.
                    check(number(brief["hiddenWords"]) < 10 and claims.get("brief") is None,
                          f"{layout}: the 300-character message, about three desk lines, is labelled: {brief}")
                if layout.startswith("phone"):
                    check(unit_of(claims, "chinese") == "characters",
                          f"{layout}: Chinese is not counted in characters: {claims.get('chinese')}")
                    check(unit_of(claims, "address") == "characters",
                          f"{layout}: the address is not counted in characters: {claims.get('address')}")
                if layout in ("phone-360", "phone-412"):
                    check(claims.get("brief") is not None, f"{layout}: the 300-character message hides a line and says nothing")
                # From review: the clamp draws a table whole, so a table alone hides nothing.
                table = next(row for row in rows if row["id"] == IDS["table"])
                check(number(table["hiddenWords"]) == 0, f"{layout}: the clamp hid part of a table: {table}")
                check(claims.get("table") is None, f"{layout}: a table the clamp draws whole says {claims.get('table')}")
                if layout.startswith("phone"):
                    for name in ("table-prose", "code", "list"):
                        check(claims.get(name) is not None, f"{layout}: the {name} row hides words and says nothing")
                if args.screenshots:
                    page.screenshot(path=str(args.screenshots / f"elision-{layout}.png"))
                # The switch off: the agent's row is counted from its text as written.
                page.evaluate("() => applyRichHeaders(false)")
                page.wait_for_timeout(150)
                off: object = page.evaluate(MEASURE_JS)
                assert isinstance(off, list)
                judge(f"{layout} rich headers off", off, notes)
                check(not errors, f"{layout}: the page threw: {errors}")
                page.context.close()
            version = browser.version
        finally:
            browser.close()
    print(f"Chromium {version}: in seven layouts every folded row's one label claimed no more than its clamp"
          " hides, on a line as wide as the text and at its size; the wall was labelled with at least half of"
          " what it hides; at 1280 the 300-character message, about three lines, said nothing; Chinese and"
          " a bare address were counted in characters; a table the clamp draws whole said nothing, and a table"
          " under prose, a code block and a list of short items were labelled on the phones within what they"
          " hide; with rich headers off the agent's row still held. (label/hidden per row:)")
    for note in notes:
        print(f"  {note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
