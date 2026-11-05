#!/usr/bin/env python3
"""Check the public website under `site/` before GitHub Pages serves it.

`site/` is hand-written static HTML and one stylesheet, published at
https://rrnewton.github.io/agent-utils/ by `.github/workflows/pages.yml`. Nothing builds it, so
nothing else would notice a page that no longer parses, a link to a document that was renamed, a
`#fragment` whose heading was reworded, or a stylesheet quietly fetched from another origin. This
script is that notice. It needs only the Python standard library and `git`.

What it checks. `--self-test` builds a small fixture site and, for each rule below, a variant
that breaks exactly that rule, and requires the rule's own message; it also requires the valid
fixture to pass:

* **Structure.** Every page starts with `<!DOCTYPE html>`, parses, closes every non-void element
  explicitly and in order, has unique ids, one `<main>` with an id that a skip link (the first link
  on the page) targets, `<header>`, `<nav>` and `<footer>` landmarks, exactly one `<h1>`, and no
  heading level skipped on the way down.
* **Metadata.** `<html lang>`, a non-empty `<title>`, a non-empty `<meta name="description">`, a
  viewport, and a Content-Security-Policy `<meta>` placed before any script or stylesheet. The
  policy may name only `'self'`, `'none'` and `'sha256-...'` sources, and its `script-src` must
  list the hash of every inline script, so the browser itself refuses anything from elsewhere.
* **Local links.** Every relative `href`/`src` resolves to a file inside `site/`, and a
  `#fragment` names an id on the target page. Root-relative links (`/x`) are refused: under a
  project site `/` is the account's root, not this site's.
* **Repository links.** Every `https://github.com/rrnewton/agent-utils/blob|tree/main/<path>`
  names a tracked path (a file for `blob`, a directory for `tree`) that is not a tracked symlink,
  because GitHub shows a link blob as its target path rather than the document. A `#fragment` on a
  Markdown target must match one of its headings by GitHub's slug rules.
* **One origin.** Nothing is loaded from another origin or from a `data:`/`javascript:` URL:
  scripts, stylesheets, icons, images, media, frames, CSS `url()`/`@import`, SVG references.
  Inline `style` attributes, `<style>` elements and `on*` handlers are refused; the policy would
  block them anyway.
* **Accessibility basics.** Every `<img>` has `alt`; an inline `<svg>` is either `aria-hidden` or
  has `role="img"` and an accessible name.
* **Assets.** Only known file types live in `site/`, `index.html` and `.nojekyll` exist, GIFs are
  at most 3 MB each, other images at most 1.5 MB, text assets at most 256 KB. Every published
  file is tracked by git (the index counts), because CI and the Pages deploy check out a clean
  tree: a GIF that was never `git add`-ed would pass here and be missing there.
* **What is published.** `site/demos/` holds the tools that record the site's media, and
  `site/README.md` holds the maintainer notes. Both are repository source, not website: they are
  not checked as website files, `--stage` leaves them out of what the Pages workflow uploads, and
  a page link to them is refused, because the target would not exist once published.

The pages' own content, wording and visual quality are a human's job; see `site/README.md`.

Usage:
    python3 scripts/check_site.py
    python3 scripts/check_site.py --self-test
    python3 scripts/check_site.py --stage DIR    # check, then copy the published files into DIR
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import html
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

REPO_ROOT = Path(__file__).resolve().parent.parent
SITE = "site"
REPO_URL = "https://github.com/rrnewton/agent-utils"
PAGES_URL = "https://rrnewton.github.io/agent-utils"

#: Elements that never take an end tag.
VOID = frozenset(
    {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source",
     "track", "wbr"}
)

#: (element, attribute) pairs that make the browser FETCH something. Anchors are navigation and
#: are checked separately; any `<link>` fetches (stylesheet, icon, preload, preconnect...).
RESOURCE_ATTRS = frozenset(
    {
        ("script", "src"), ("link", "href"), ("img", "src"), ("img", "srcset"),
        ("source", "src"), ("source", "srcset"), ("iframe", "src"), ("frame", "src"),
        ("embed", "src"), ("object", "data"), ("video", "src"), ("video", "poster"),
        ("audio", "src"), ("track", "src"), ("input", "src"), ("image", "href"),
        ("image", "xlink:href"), ("use", "href"), ("use", "xlink:href"), ("feimage", "href"),
        ("feimage", "xlink:href"),
    }
)
NAVIGATION_ATTRS = frozenset({("a", "href"), ("area", "href")})

MB = 1_000_000
GIF_LIMIT = 3 * MB
IMAGE_LIMIT = 1_500_000
TEXT_LIMIT = 256_000
IMAGE_SUFFIXES = frozenset({".png", ".jpg", ".jpeg", ".webp", ".avif", ".ico"})
TEXT_SUFFIXES = frozenset({".html", ".css", ".svg", ".js", ".txt", ".md"})
#: Files allowed without a known suffix.
NAMED_FILES = frozenset({".nojekyll"})
#: Top-level entries of `site/` that are repository source, never published: `demos/` holds the
#: tools that record the site's media (a Python recorder, a mock server, a config template), and
#: `README.md` is the maintainer notes, which GitHub shows when browsing `site/`.
UNPUBLISHED = frozenset({"demos", "README.md"})

#: CSP source expressions that keep every load on this origin.
CSP_ALLOWED_SOURCE = re.compile(r"^(?:'self'|'none'|'sha256-[A-Za-z0-9+/]+={0,2}')$")
CSP_FETCH_DIRECTIVES = frozenset(
    {"default-src", "script-src", "style-src", "img-src", "font-src", "connect-src",
     "media-src", "object-src", "frame-src", "child-src", "worker-src", "manifest-src",
     "base-uri", "form-action"}
)

HEADING_TAGS = {f"h{level}": level for level in range(1, 7)}


@dataclass(frozen=True)
class Repo:
    """The repository facts link checks depend on, injectable so the self-test needs no git."""

    root: Path
    tracked: frozenset[str]
    symlinks: frozenset[str]

    def is_directory(self, path: str) -> bool:
        prefix = path.rstrip("/") + "/"
        return path.strip("/") == "" or any(name.startswith(prefix) for name in self.tracked)


@dataclass
class Reference:
    """One URL-valued attribute on a page."""

    tag: str
    attr: str
    value: str
    line: int
    resource: bool


@dataclass
class Page:
    """What one parse of one HTML file found."""

    path: Path
    problems: list[str] = field(default_factory=list)
    ids: set[str] = field(default_factory=set)
    references: list[Reference] = field(default_factory=list)
    inline_scripts: list[tuple[int, str]] = field(default_factory=list)
    csp: str | None = None
    csp_line: int = 0
    first_resource_line: int = 0
    lang: str | None = None
    title: str | None = None
    description: str | None = None
    has_viewport: bool = False
    has_doctype: bool = False
    landmarks: dict[str, int] = field(default_factory=dict)
    main_id: str | None = None
    first_link: str | None = None
    headings: list[tuple[int, int]] = field(default_factory=list)


class _PageParser(HTMLParser):
    """Collect structure, metadata and URL references; report balance problems as they occur."""

    def __init__(self, page: Page) -> None:
        super().__init__(convert_charrefs=True)
        self.page = page
        self.stack: list[tuple[str, int]] = []
        self.svg_depth = 0
        self.in_head = False
        self.title_parts: list[str] | None = None
        self.script_parts: list[str] | None = None
        self.script_line = 0
        #: Open top-level inline <svg> elements: (line, has an accessible name, aria-labelledby).
        self.svg_names: list[tuple[int, bool, str | None]] = []

    def _problem(self, message: str, line: int | None = None) -> None:
        where = line if line is not None else self.getpos()[0]
        self.page.problems.append(f"{where}: {message}")

    def handle_decl(self, decl: str) -> None:
        if decl.strip().lower() == "doctype html":
            self.page.has_doctype = True

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self._open(tag, attrs, self_closing=False)

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag not in VOID and self.svg_depth == 0 and tag != "svg":
            self._problem(
                f"<{tag}/> is not self-closing in HTML; close it with </{tag}> instead"
            )
        self._open(tag, attrs, self_closing=True)

    def _open(self, tag: str, attrs: list[tuple[str, str | None]], *, self_closing: bool) -> None:
        line = self.getpos()[0]
        values = {name: (value if value is not None else "") for name, value in attrs}
        page = self.page
        self._check_attributes(tag, attrs, values)

        if tag == "html":
            page.lang = values.get("lang", "").strip() or None
        elif tag == "head":
            self.in_head = True
        elif tag == "title" and self.in_head and self.svg_depth == 0:
            self.title_parts = []
        elif tag == "meta":
            self._meta(values, line)
        elif tag == "base":
            self._problem("<base> changes how every relative link resolves; remove it")
        elif tag == "style":
            self._problem("<style> element: keep all CSS in the one stylesheet")
        elif tag == "script":
            if "src" not in values:
                self.script_parts = []
                self.script_line = line
            if not page.first_resource_line:
                page.first_resource_line = line
        elif tag == "link":
            rel = values.get("rel", "").lower().split()
            if "canonical" in rel:
                self._problem("<link rel=canonical>: pages link to each other relatively")
            if not page.first_resource_line:
                page.first_resource_line = line
        elif tag in ("header", "nav", "footer", "main"):
            page.landmarks[tag] = page.landmarks.get(tag, 0) + 1
            if tag == "main":
                page.main_id = values.get("id") or None
        elif tag == "a" and page.first_link is None and "href" in values:
            page.first_link = values["href"]
        elif tag == "img" and "alt" not in values:
            self._problem("<img> without alt (use alt=\"\" only for a purely decorative image)")
        if tag in HEADING_TAGS and self.svg_depth == 0:
            page.headings.append((HEADING_TAGS[tag], line))

        if tag == "svg":
            if self.svg_depth == 0:
                hidden = values.get("aria-hidden") == "true"
                named = values.get("role") == "img" and bool(
                    values.get("aria-label", "").strip() or values.get("aria-labelledby")
                )
                self.svg_names.append((line, hidden or named, values.get("aria-labelledby")))
            self.svg_depth += 1

        for name, value in attrs:
            if value is None:
                continue
            if (tag, name) in RESOURCE_ATTRS:
                page.references.append(Reference(tag, name, value, line, True))
            elif (tag, name) in NAVIGATION_ATTRS:
                page.references.append(Reference(tag, name, value, line, False))

        if self_closing:
            if tag == "svg":
                self._close_svg()
            return
        if tag not in VOID:
            self.stack.append((tag, line))

    def _check_attributes(
        self, tag: str, attrs: list[tuple[str, str | None]], values: dict[str, str]
    ) -> None:
        for name, _value in attrs:
            if name == "style":
                self._problem(f"inline style attribute on <{tag}>: use a class in the stylesheet")
            elif name.startswith("on"):
                self._problem(f"inline event handler {name}= on <{tag}>: the page policy blocks it")
        element_id = values.get("id")
        if element_id is not None:
            if not element_id:
                self._problem(f"empty id on <{tag}>")
            elif element_id in self.page.ids:
                self._problem(f"duplicate id {element_id!r}")
            self.page.ids.add(element_id)
        name_anchor = values.get("name")
        if tag == "a" and name_anchor:
            self.page.ids.add(name_anchor)

    def _meta(self, values: dict[str, str], line: int) -> None:
        page = self.page
        name = values.get("name", "").lower()
        if name == "description":
            page.description = values.get("content", "").strip() or None
        elif name == "viewport":
            page.has_viewport = bool(values.get("content", "").strip())
        if values.get("http-equiv", "").lower() == "content-security-policy":
            if page.csp is not None:
                self._problem("more than one Content-Security-Policy <meta>")
            page.csp = values.get("content", "")
            page.csp_line = line

    def handle_endtag(self, tag: str) -> None:
        line = self.getpos()[0]
        if tag in VOID:
            self._problem(f"</{tag}>: <{tag}> is a void element and takes no end tag")
            return
        if not self.stack or self.stack[-1][0] != tag:
            if any(open_tag == tag for open_tag, _ in self.stack):
                while self.stack and self.stack[-1][0] != tag:
                    open_tag, open_line = self.stack.pop()
                    self._problem(f"<{open_tag}> opened on line {open_line} is not closed "
                                  f"before </{tag}>", line)
                self.stack.pop()
            else:
                self._problem(f"</{tag}> closes nothing that is open")
                return
        else:
            self.stack.pop()
        if tag == "head":
            self.in_head = False
        elif tag == "title" and self.title_parts is not None:
            self.page.title = "".join(self.title_parts).strip() or None
            self.title_parts = None
        elif tag == "script" and self.script_parts is not None:
            self.page.inline_scripts.append((self.script_line, "".join(self.script_parts)))
            self.script_parts = None
        elif tag == "svg":
            self._close_svg()

    def _close_svg(self) -> None:
        self.svg_depth -= 1
        if self.svg_depth == 0 and self.svg_names:
            line, named, labelledby = self.svg_names.pop()
            if labelledby:
                missing = [ref for ref in labelledby.split() if ref not in self.page.ids]
                if missing:
                    self._problem(f"<svg> aria-labelledby names missing id(s) {missing}", line)
            if not named:
                self._problem(
                    "inline <svg> needs aria-hidden=\"true\" (decorative) or role=\"img\" with "
                    "aria-label or aria-labelledby",
                    line,
                )

    def handle_data(self, data: str) -> None:
        if self.title_parts is not None:
            self.title_parts.append(data)
        if self.script_parts is not None:
            self.script_parts.append(data)

    def close(self) -> None:
        super().close()
        for tag, line in self.stack:
            self._problem(f"<{tag}> is never closed", line)
        self.stack.clear()


def parse_page(path: Path, text: str) -> Page:
    page = Page(path)
    parser = _PageParser(page)
    parser.feed(text)
    parser.close()
    return page


# --------------------------------------------------------------------------------------------
# GitHub Markdown anchors


def github_slug(text: str) -> str:
    """GitHub's heading slug: lowercase, drop punctuation and symbols, spaces become hyphens.

    Letters, numbers, combining marks, `-` and `_` survive; everything else is removed, and each
    space (not each run of spaces) becomes one hyphen, which is why an em dash between two spaces
    leaves `--`.
    """

    kept = [
        char
        for char in text.lower()
        if char in " -_" or unicodedata.category(char)[0] in "LNM"
    ]
    return "".join(kept).replace(" ", "-")


_CODE_SPAN = re.compile(r"(`+)(.+?)\1")
_IMAGE = re.compile(r"!\[([^\]]*)\]\([^)]*\)")
_LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)|\[([^\]]*)\]\[[^\]]*\]")
_AUTOLINK = re.compile(r"<((?:https?|mailto):[^>]+)>")
_TAG = re.compile(r"<[^>]+>")
_EMPHASIS_UNDERSCORE = re.compile(r"(?<![A-Za-z0-9])_+|_+(?![A-Za-z0-9])")
_ESCAPE = re.compile(r"\\([!-/:-@\[-`{-~])")
_HTML_ANCHOR = re.compile(r"""<[A-Za-z][^>]*?\s(?:id|name)\s*=\s*["']([^"']+)["']""")
_ATX = re.compile(r"^ {0,3}(#{1,6})(?:[ \t]+(.*?))?[ \t]*$")
_FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
_SETEXT = re.compile(r"^ {0,3}(=+|-+)[ \t]*$")
#: One block-quote marker or list-item marker at the start of a line. GitHub gives a heading inside
#: a block quote (`> ### Note`) or a list item (`- ## Step`) an anchor like any other heading.
_CONTAINER = re.compile(r"^ {0,3}(?:>[ \t]?|(?:[-+*]|\d{1,9}[.)])[ \t]+)")


def _strip_containers(line: str) -> str:
    """`line` without its leading block-quote and list-item markers."""

    while True:
        match = _CONTAINER.match(line)
        if not match or not match.end():
            return line
        line = line[match.end():]


def heading_text(markdown: str) -> str:
    """The rendered text of a heading's inline Markdown, as GitHub slugs it."""

    pieces: list[str] = []
    position = 0
    for match in _CODE_SPAN.finditer(markdown):
        pieces.append(_strip_inline(markdown[position:match.start()]))
        pieces.append(match.group(2).strip())
        position = match.end()
    pieces.append(_strip_inline(markdown[position:]))
    return "".join(pieces)


def _strip_inline(text: str) -> str:
    text = _IMAGE.sub(r"\1", text)
    text = _LINK.sub(lambda match: match.group(1) or match.group(2) or "", text)
    text = _AUTOLINK.sub(r"\1", text)
    text = _TAG.sub("", text)
    text = text.replace("*", "")
    text = _EMPHASIS_UNDERSCORE.sub("", text)
    text = _ESCAPE.sub(r"\1", text)
    return html.unescape(text)


def markdown_anchors(text: str) -> set[str]:
    """Every fragment GitHub would accept for a Markdown file: heading slugs plus HTML ids."""

    lines = text.splitlines()
    start = 0
    if lines and lines[0].strip() == "---":  # YAML front matter renders as a table, not headings
        for index in range(1, len(lines)):
            if lines[index].strip() in ("---", "..."):
                start = index + 1
                break
    anchors: set[str] = set()
    counts: dict[str, int] = {}
    fence: str | None = None
    previous: str | None = None

    def add(raw: str) -> None:
        slug = github_slug(heading_text(raw.strip()))
        if not slug:
            return
        if slug not in anchors:
            anchors.add(slug)
            return
        number = counts.get(slug, 0)
        while f"{slug}-{number + 1}" in anchors:
            number += 1
        number += 1
        counts[slug] = number
        anchors.add(f"{slug}-{number}")

    for line in lines[start:]:
        # Fences and ATX headings are recognized inside block quotes and list items too; setext
        # headings only at the top level, which is where this repository's documents use them.
        body = _strip_containers(line)
        fence_match = _FENCE.match(body)
        if fence is not None:
            if fence_match and fence_match.group(1)[0] == fence[0] and len(
                fence_match.group(1)
            ) >= len(fence) and not body.strip().strip(fence[0]):
                fence = None
            previous = None
            continue
        if fence_match:
            fence = fence_match.group(1)
            previous = None
            continue
        anchors.update(_HTML_ANCHOR.findall(line))
        atx = _ATX.match(body)
        if atx:
            content = atx.group(2) or ""
            content = re.sub(r"(?:^|[ \t]+)#+$", "", content)
            add(content)
            previous = None
            continue
        if (
            previous is not None
            and _SETEXT.match(line)
            and "|" not in previous
            and not _SETEXT.match(previous)
        ):
            add(previous)
            previous = None
            continue
        previous = line if line.strip() and not line.startswith("    ") else None
    return anchors


# --------------------------------------------------------------------------------------------
# Whole-site checks


@dataclass
class Report:
    problems: list[str] = field(default_factory=list)
    pages: int = 0
    local_links: int = 0
    repo_links: int = 0
    files: int = 0


def _rel(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return path.as_posix()


def _inside(path: Path, directory: Path) -> bool:
    try:
        path.relative_to(directory)
    except ValueError:
        return False
    return True


def _unpublished(path: Path, site: Path) -> str | None:
    """The `UNPUBLISHED` directory `path` lies in, or None; both paths resolved or both not."""

    try:
        parts = path.relative_to(site).parts
    except ValueError:
        return None
    return parts[0] if parts and parts[0] in UNPUBLISHED else None


def published_files(site: Path) -> list[Path]:
    """Every file under `site` that GitHub Pages serves, symlinks included so they are refused."""

    return sorted(
        path
        for path in site.rglob("*")
        if (path.is_file() or path.is_symlink()) and _unpublished(path, site) is None
    )


def _resolve_local(
    base_file: Path, target: str, site: Path
) -> tuple[Path | None, str | None]:
    """Resolve a relative URL path against `base_file`; return (file, problem)."""

    if target.startswith("/"):
        return None, (
            f"root-relative link {target!r}: under a project site '/' is the account root; "
            "use a relative link"
        )
    if not target:
        return base_file, None
    resolved = (base_file.parent / unquote(target)).resolve()
    if not _inside(resolved, site.resolve()):
        return None, f"{target!r} leaves {SITE}/; link repository files by their GitHub URL"
    hidden = _unpublished(resolved, site.resolve())
    if hidden is not None:
        return None, (
            f"{target!r} is under {SITE}/{hidden}, which is repository source and is not "
            "published; link it by its GitHub URL"
        )
    if resolved.is_dir():
        resolved = resolved / "index.html"
    if not resolved.is_file():
        return None, f"{target!r} does not resolve to a file in {SITE}/"
    return resolved, None


def check_repository_link(url: str, repo: Repo, markdown_cache: dict[str, set[str]]) -> str | None:
    """Return a problem with one `REPO_URL` link, or None when it names what it should."""

    parts = urlsplit(url)
    path = parts.path
    prefix = urlsplit(REPO_URL).path
    rest = path[len(prefix):].lstrip("/")
    kind, _, after = rest.partition("/")
    if kind not in ("blob", "tree"):
        return None  # the repository page, issues, actions: navigation, nothing to resolve
    ref, _, target = after.partition("/")
    if ref != "main":
        return f"{url}: link {kind}/main/..., not {kind}/{ref or '(nothing)'}/"
    if parts.query:
        return f"{url}: drop the query string; link the plain document"
    target = unquote(target)
    fragment = unquote(parts.fragment)
    if kind == "tree":
        if target.strip("/") and not repo.is_directory(target):
            if target.rstrip("/") in repo.tracked:
                return f"{url}: {target!r} is a file; use /blob/main/"
            return f"{url}: no tracked directory {target!r}"
        if fragment:
            return f"{url}: a directory listing has no #{fragment}; link the README's blob URL"
        return None
    if not target or target.endswith("/"):
        return f"{url}: a blob link must name a file"
    if target not in repo.tracked:
        if repo.is_directory(target):
            return f"{url}: {target!r} is a directory; use /tree/main/"
        return f"{url}: {target!r} is not a tracked file"
    if target in repo.symlinks:
        return (
            f"{url}: {target!r} is a tracked symlink, which GitHub shows as its target path "
            "rather than the document; link the file it points to"
        )
    if not fragment:
        return None
    if target.lower().endswith((".md", ".markdown")):
        if target not in markdown_cache:
            try:
                text = (repo.root / target).read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError) as error:
                return f"{url}: cannot read {target!r} to check #{fragment}: {error}"
            markdown_cache[target] = markdown_anchors(text)
        if fragment not in markdown_cache[target]:
            return f"{url}: {target!r} has no heading or anchor #{fragment}"
        return None
    if not re.fullmatch(r"L\d+(?:-L\d+)?", fragment):
        return f"{url}: #{fragment} on a non-Markdown file must be a line anchor such as #L10"
    return None


def _check_reference(
    page: Page,
    ref: Reference,
    site: Path,
    pages: dict[Path, Page],
    repo: Repo,
    markdown_cache: dict[str, set[str]],
    report: Report,
) -> list[str]:
    value = ref.value.strip()
    problems: list[str] = []
    what = f"<{ref.tag} {ref.attr}>"
    candidates = (
        [item.strip().split()[0] for item in value.split(",") if item.strip()]
        if ref.attr == "srcset"
        else [value]
    )
    for candidate in candidates:
        parts = urlsplit(candidate)
        scheme = parts.scheme.lower()
        external = scheme in ("http", "https") or candidate.startswith("//")
        if ref.resource:
            if external:
                problems.append(f"{what} loads {candidate!r} from another origin")
                continue
            if scheme:
                problems.append(f"{what} {candidate!r}: keep assets as files in {SITE}/")
                continue
        else:
            if scheme == "mailto":
                continue
            if scheme == "http" or candidate.startswith("//"):
                problems.append(f"{what} {candidate!r}: use an https:// URL")
                continue
            if scheme == "https":
                if candidate.startswith(PAGES_URL):
                    problems.append(f"{what} {candidate!r}: link this site's pages relatively")
                elif candidate == REPO_URL or candidate.startswith(REPO_URL + "/"):
                    report.repo_links += 1
                    problem = check_repository_link(candidate, repo, markdown_cache)
                    if problem:
                        problems.append(problem)
                continue
            if scheme:
                problems.append(f"{what} {candidate!r}: unsupported URL scheme {scheme!r}")
                continue
            if candidate == "#":
                problems.append(f"{what} '#' goes nowhere")
                continue
        report.local_links += 1
        target, problem = _resolve_local(page.path, parts.path, site)
        if problem:
            problems.append(f"{what} {problem}")
            continue
        if parts.fragment and target is not None:
            fragment = unquote(parts.fragment)
            target_page = pages.get(target)
            if target_page is None:
                problems.append(f"{what} {candidate!r}: #{fragment} on a file that is not a page")
            elif fragment not in target_page.ids:
                problems.append(
                    f"{what} {candidate!r}: {_rel(target, site)} has no id {fragment!r}"
                )
    return problems


def _sha256_source(script: str) -> str:
    digest = hashlib.sha256(script.encode("utf-8")).digest()
    return "'sha256-" + base64.b64encode(digest).decode("ascii") + "'"


def _check_csp(page: Page) -> list[str]:
    if page.csp is None:
        return ["no Content-Security-Policy <meta>; copy the one from index.html"]
    problems: list[str] = []
    if page.first_resource_line and page.csp_line > page.first_resource_line:
        problems.append(
            "the Content-Security-Policy <meta> must come before the first script or stylesheet"
        )
    directives: dict[str, list[str]] = {}
    for raw in page.csp.split(";"):
        tokens = raw.split()
        if tokens:
            directives[tokens[0].lower()] = tokens[1:]
    if directives.get("default-src") not in (["'none'"], ["'self'"]):
        problems.append("the page policy must set default-src 'none' or 'self'")
    for name, sources in directives.items():
        if name not in CSP_FETCH_DIRECTIVES:
            continue
        for source in sources:
            if not CSP_ALLOWED_SOURCE.match(source):
                problems.append(
                    f"page policy {name} allows {source!r}; only 'self', 'none' and script "
                    "hashes keep every load on this origin"
                )
    script_sources = directives.get("script-src", directives.get("default-src", []))
    for line, script in page.inline_scripts:
        expected = _sha256_source(script)
        if expected not in script_sources:
            problems.append(f"{line}: inline script is not allowed by the page policy; add "
                            f"{expected} to script-src")
        if re.search(r"(?:https?:)?//[A-Za-z0-9]", script):
            problems.append(f"{line}: inline script contains a URL; scripts load nothing")
    return problems


def _check_page_shape(page: Page) -> list[str]:
    problems: list[str] = []
    if not page.has_doctype:
        problems.append("missing <!DOCTYPE html> (without it browsers render in quirks mode)")
    if not page.lang:
        problems.append("<html> has no lang")
    if not page.title:
        problems.append("missing or empty <title>")
    if not page.description:
        problems.append("missing or empty <meta name=\"description\">")
    if not page.has_viewport:
        problems.append("missing <meta name=\"viewport\">; the page will not fit a phone")
    for landmark in ("header", "nav", "footer"):
        if not page.landmarks.get(landmark):
            problems.append(f"no <{landmark}> landmark")
    if page.landmarks.get("main", 0) != 1:
        problems.append(f"expected exactly one <main>, found {page.landmarks.get('main', 0)}")
    elif not page.main_id:
        problems.append("<main> has no id for the skip link to target")
    elif page.first_link != f"#{page.main_id}":
        problems.append(
            f"the first link on the page must be the skip link to #{page.main_id}, "
            f"found {page.first_link!r}"
        )
    levels = [level for level, _ in page.headings]
    if levels.count(1) != 1:
        problems.append(f"expected exactly one <h1>, found {levels.count(1)}")
    if levels and levels[0] != 1:
        problems.append(f"{page.headings[0][1]}: the first heading must be the <h1>")
    for (before, _), (level, line) in zip(page.headings, page.headings[1:]):
        if level > before + 1:
            problems.append(f"{line}: <h{level}> follows <h{before}>, skipping a level")
    return problems


_CSS_COMMENT = re.compile(r"/\*.*?\*/", re.S)
_CSS_URL = re.compile(r"""url\(\s*(['"]?)([^'")]*)\1\s*\)""", re.I)
_CSS_IMPORT = re.compile(r"""@import\s+(?:url\()?\s*['"]?([^'");\s]+)""", re.I)
_SVG_REF = re.compile(r"""\s(?:xlink:href|href|src)\s*=\s*["']([^"']*)["']""")


def _check_css_or_svg(path: Path, text: str, site: Path) -> list[str]:
    problems: list[str] = []
    refs: list[str] = []
    stripped = _CSS_COMMENT.sub("", text)
    refs.extend(match.group(2) for match in _CSS_URL.finditer(stripped))
    refs.extend(match.group(1) for match in _CSS_IMPORT.finditer(stripped))
    if path.suffix == ".svg":
        refs.extend(_SVG_REF.findall(text))
    for ref in refs:
        value = ref.strip()
        if not value or value.startswith("#"):
            continue
        parts = urlsplit(value)
        if parts.scheme or value.startswith("//"):
            problems.append(f"references {value!r}; everything loads from {SITE}/")
            continue
        _target, problem = _resolve_local(path, parts.path, site)
        if problem:
            problems.append(problem)
    return problems


def _check_assets(site: Path, files: Sequence[Path]) -> list[str]:
    problems: list[str] = []
    names = {_rel(path, site) for path in files}
    for required in ("index.html", ".nojekyll"):
        if required not in names:
            problems.append(f"{SITE}/{required} is missing")
    for path in files:
        name = _rel(path, site)
        suffix = path.suffix.lower()
        size = path.stat().st_size
        if path.name in NAMED_FILES:
            continue
        if suffix == ".gif":
            limit = GIF_LIMIT
        elif suffix in IMAGE_SUFFIXES:
            limit = IMAGE_LIMIT
        elif suffix in TEXT_SUFFIXES:
            limit = TEXT_LIMIT
        else:
            problems.append(
                f"{SITE}/{name}: {suffix or 'no suffix'} is not a known website file type"
            )
            continue
        if size > limit:
            problems.append(f"{SITE}/{name}: {size:,} bytes exceeds the {limit:,}-byte cap")
    return problems


def _check_tracked(files: Sequence[Path], repo: Repo) -> list[str]:
    """Every published file must be in the git index: CI and Pages build from a clean checkout."""

    problems: list[str] = []
    for path in files:
        name = _rel(path, repo.root)
        if name not in repo.tracked:
            problems.append(
                f"{name}: not tracked by git, so a clean checkout (CI, the Pages deploy) would "
                "not have it; `git add` it or remove it"
            )
    return problems


def check_site(site: Path, repo: Repo) -> Report:
    """Run every check over `site`; problems are reported relative to the repository root."""

    report = Report()
    if not site.is_dir():
        report.problems.append(f"{site} is not a directory")
        return report
    files = published_files(site)
    report.files = len(files)
    root = repo.root
    for path in files:
        if path.is_symlink():
            report.problems.append(f"{_rel(path, root)}: symlinks do not survive the Pages upload")
    files = [path for path in files if not path.is_symlink()]
    report.problems.extend(_check_assets(site, files))
    report.problems.extend(_check_tracked(files, repo))

    pages: dict[Path, Page] = {}
    for path in files:
        if path.suffix.lower() == ".html":
            text = path.read_text(encoding="utf-8")
            pages[path.resolve()] = parse_page(path.resolve(), text)
    report.pages = len(pages)

    markdown_cache: dict[str, set[str]] = {}
    for resolved, page in sorted(pages.items()):
        label = _rel(resolved, root.resolve())
        found = list(page.problems)
        found.extend(_check_page_shape(page))
        found.extend(_check_csp(page))
        for ref in page.references:
            for problem in _check_reference(page, ref, site, pages, repo, markdown_cache, report):
                found.append(f"{ref.line}: {problem}")
        report.problems.extend(f"{label}:{problem}" for problem in found)

    for path in files:
        if path.suffix.lower() in (".css", ".svg"):
            text = path.read_text(encoding="utf-8")
            label = _rel(path, root)
            report.problems.extend(
                f"{label}: {problem}" for problem in _check_css_or_svg(path, text, site)
            )
    return report


def stage(site: Path, destination: Path) -> int:
    """Copy exactly the published files of `site` into `destination`; return how many.

    Run only after `check_site` passed, so there is no symlink to copy and every file is tracked.
    The destination must be absent or empty: files left over from something else would be
    published too.
    """

    if destination.exists() and (not destination.is_dir() or any(destination.iterdir())):
        raise SystemExit(f"check-site: --stage {destination} must be absent or an empty directory")
    files = published_files(site)
    for path in files:
        target = destination / path.relative_to(site)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    return len(files)


def _git_lines(argv: Sequence[str]) -> list[str]:
    done = subprocess.run(argv, cwd=REPO_ROOT, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        raise SystemExit(f"check-site: `{' '.join(argv)}` failed:\n{done.stderr.strip()}")
    return [name for name in done.stdout.split("\0") if name]


def repository() -> Repo:
    """Tracked paths (the index, so a staged new document counts) and tracked symlinks."""

    staged = _git_lines(("git", "ls-files", "-z", "--stage"))
    tracked: set[str] = set()
    symlinks: set[str] = set()
    for entry in staged:
        meta, _, name = entry.partition("\t")
        tracked.add(name)
        if meta.startswith("120000 "):
            symlinks.add(name)
    return Repo(REPO_ROOT, frozenset(tracked), frozenset(symlinks))


# --------------------------------------------------------------------------------------------
# Self-test

_GOOD_SCRIPT = "document.documentElement.classList.add('js');"


def _page(
    *,
    title: str = "Fixture",
    body: str = "",
    head_extra: str = "",
    lang: str = ' lang="en"',
    script: str = _GOOD_SCRIPT,
    csp: str | None = None,
    doctype: str = "<!DOCTYPE html>\n",
    description: str = '<meta name="description" content="A fixture page.">',
) -> str:
    policy = (
        csp
        if csp is not None
        else f"default-src 'none'; script-src {_sha256_source(script)}; style-src 'self'; "
        "img-src 'self'"
    )
    return (
        f"{doctype}<html{lang}>\n<head>\n<meta charset=\"utf-8\">\n"
        f"<meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">\n"
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n"
        f"<title>{title}</title>\n{description}\n<script>{script}</script>\n"
        f"<link rel=\"stylesheet\" href=\"style.css\">\n{head_extra}</head>\n<body>\n"
        "<a class=\"skip\" href=\"#main\">Skip to content</a>\n"
        "<header><nav><a href=\"index.html\">Home</a> <a href=\"other.html#part\">Other</a>"
        "</nav></header>\n<main id=\"main\">\n<h1>Fixture</h1>\n"
        f"{body}\n</main>\n<footer><p>Footer</p></footer>\n</body>\n</html>\n"
    )


_FIXTURE_MARKDOWN = """---
name: fixture
description: front matter is a table, not a heading
---

# Fixture guide

## Phase 1: Dependencies

## Trust and safety model — read before widening allow

## `agentctl` (skill)

## Repeated

## Repeated

Setext heading
--------------

<a id="explicit-anchor"></a>

```text
## Inside a fence
```

> ### Quoted heading

- ## Listed heading

> ```text
> ## Inside a quoted fence
> ```
"""


def _fixture(root: Path) -> Repo:
    site = root / SITE
    site.mkdir()
    (site / "index.html").write_text(
        _page(
            body=(
                '<p><a href="https://github.com/rrnewton/agent-utils/blob/main/docs/GUIDE.md'
                '#phase-1-dependencies">guide</a> '
                '<a href="https://github.com/rrnewton/agent-utils/tree/main/docs">docs</a> '
                '<a href="https://github.com/rrnewton/agent-utils/issues">issues</a> '
                '<a href="https://example.org/elsewhere">elsewhere</a> '
                '<a href="mailto:someone@example.org">mail</a></p>\n'
                '<img src="mark.svg" alt="">\n'
                '<svg aria-hidden="true" viewBox="0 0 2 2"><circle cx="1" cy="1" r="1"/></svg>\n'
                '<h2 id="part">Part</h2>'
            )
        ),
        encoding="utf-8",
    )
    (site / "other.html").write_text(
        _page(title="Other", body='<h2 id="part">Part</h2><p><a href="./#part">back</a></p>'),
        encoding="utf-8",
    )
    (site / "style.css").write_text(
        "/* url(https://comment.example/ignored.png) */\n.mark{background:url(mark.svg)}\n",
        encoding="utf-8",
    )
    (site / "mark.svg").write_text(
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 2"><circle r="1"/></svg>\n',
        encoding="utf-8",
    )
    (site / ".nojekyll").write_text("", encoding="utf-8")
    (site / "README.md").write_text("# Maintainer notes\n", encoding="utf-8")
    (root / "docs").mkdir()
    (root / "docs" / "GUIDE.md").write_text(_FIXTURE_MARKDOWN, encoding="utf-8")
    return _tracking_site(
        root,
        Repo(
            root,
            frozenset({"docs/GUIDE.md", "docs/LINK.md", "docs/tool.py", "README.md"}),
            frozenset({"docs/LINK.md"}),
        ),
    )


def _tracking_site(root: Path, repo: Repo) -> Repo:
    """`repo` with every file now under `root/site` tracked, as `git add` would leave it."""

    names = {
        path.relative_to(root).as_posix()
        for path in (root / SITE).rglob("*")
        if path.is_file() and not path.is_symlink()
    }
    return Repo(repo.root, repo.tracked | names, repo.symlinks)


def self_test() -> int:
    """Prove every check passes valid input and fails the defect it exists to catch."""

    failures: list[str] = []
    controls = 0

    def expect(condition: bool, why: str) -> None:
        nonlocal controls
        controls += 1
        if not condition:
            failures.append(why)

    # The slug rules are the part most likely to be subtly wrong.
    for heading, slug in (
        ("Phase 1: Dependencies", "phase-1-dependencies"),
        ("Trust and safety model — read before widening allow",
         "trust-and-safety-model--read-before-widening-allow"),
        ("`agentctl` (skill)", "agentctl-skill"),
        ("Status: what works, what is stubbed", "status-what-works-what-is-stubbed"),
        ("[Linked](x.md) **bold** _em_ snake_case", "linked-bold-em-snake_case"),
        ("3. Related work", "3-related-work"),
    ):
        got = github_slug(heading_text(heading))
        expect(got == slug, f"slug of {heading!r}: want {slug!r}, got {got!r}")
    anchors = markdown_anchors(_FIXTURE_MARKDOWN)
    expect("repeated" in anchors and "repeated-1" in anchors, "a repeated heading gets -1")
    expect("setext-heading" in anchors, "a setext heading is a heading")
    expect("explicit-anchor" in anchors, "an HTML id in Markdown is an anchor")
    expect("inside-a-fence" not in anchors, "a heading inside a code fence is not a heading")
    expect("quoted-heading" in anchors, "a heading inside a block quote is a heading")
    expect("listed-heading" in anchors, "a heading inside a list item is a heading")
    expect("inside-a-quoted-fence" not in anchors,
           "a heading inside a code fence inside a block quote is not a heading")
    expect("name-fixture" not in anchors and "description-front-matter-is-a-table-not-a-heading"
           not in anchors, "YAML front matter produces no heading")

    def run(mutate: Callable[[Path, Repo], Repo | None]) -> list[str]:
        """Check the fixture after `mutate`. Files it adds count as tracked, as though the
        author ran `git add`, unless it returns the `Repo` to check against instead."""
        with tempfile.TemporaryDirectory(prefix="check-site-self-test-") as raw:
            root = Path(raw)
            repo = _fixture(root)
            changed = mutate(root / SITE, repo)
            return check_site(root / SITE, changed or _tracking_site(root, repo)).problems

    clean = run(lambda site, repo: None)
    expect(clean == [], f"the valid fixture must pass, got {clean}")

    def page_with(**kwargs: str) -> Callable[[Path, Repo], Repo | None]:
        def mutate(site: Path, repo: Repo) -> Repo | None:
            (site / "index.html").write_text(_page(**kwargs), encoding="utf-8")
            return None
        return mutate

    def body(markup: str) -> Callable[[Path, Repo], Repo | None]:
        return page_with(body=markup)

    def link(url: str) -> Callable[[Path, Repo], Repo | None]:
        return body(f'<p><a href="{url}">x</a></p>')

    def truncated(site: Path, repo: Repo) -> Repo | None:
        text = _page()
        (site / "index.html").write_text(text[: text.index("</body>")], encoding="utf-8")
        return None

    def edited(change: Callable[[str], str]) -> Callable[[Path, Repo], Repo | None]:
        """The valid index page with `change` applied to its text."""
        def mutate(site: Path, repo: Repo) -> Repo | None:
            text = _page(body='<h2 id="part">Part</h2>')
            changed = change(text)
            assert changed != text, "the edit must change the page"
            (site / "index.html").write_text(changed, encoding="utf-8")
            return None
        return mutate

    def csp_line(text: str) -> str:
        return next(line for line in text.splitlines(keepends=True)
                    if "Content-Security-Policy" in line)

    def symlink(site: Path, repo: Repo) -> Repo | None:
        (site / "alias.css").symlink_to("style.css")
        return None

    def untracked(site: Path, repo: Repo) -> Repo | None:
        (site / "clip.gif").write_bytes(b"GIF89a")
        return repo  # the fixture's own index, which does not have clip.gif

    cases: list[tuple[str, Callable[[Path, Repo], Repo | None], str]] = [
        ("an element closed out of order", body("<div><p>x</div>"), "is not closed before"),
        ("a stray end tag", body("<p>x</p></section>"), "closes nothing"),
        ("an element still open at the end of the file", truncated, "is never closed"),
        ("an end tag on a void element", body("<p>x<br></br></p>"), "void element"),
        ("self-closing syntax on a non-void HTML element", body("<div/>"), "not self-closing"),
        ("a duplicate id", body('<p id="main">x</p>'), "duplicate id"),
        ("a missing doctype", page_with(doctype=""), "DOCTYPE"),
        ("a missing lang", page_with(lang=""), "no lang"),
        ("an empty title", page_with(title=" "), "<title>"),
        ("a missing description", page_with(description=""), "description"),
        ("a missing policy", page_with(csp="", head_extra=""), "default-src"),
        ("an inline script the policy does not allow",
         page_with(csp="default-src 'none'; style-src 'self'"), "not allowed by the page policy"),
        ("a policy that allows another origin",
         page_with(csp=f"default-src 'none'; script-src {_sha256_source(_GOOD_SCRIPT)}; "
                   "img-src https://cdn.example"), "allows 'https://cdn.example'"),
        ("unsafe-inline in the policy",
         page_with(csp=f"default-src 'none'; script-src {_sha256_source(_GOOD_SCRIPT)}; "
                   "style-src 'unsafe-inline'"), "allows \"'unsafe-inline'\""),
        ("an inline script that names a URL",
         page_with(script="fetch('https://tracker.example/p');"), "contains a URL"),
        ("a link to a missing page", link("missing.html"), "does not resolve"),
        ("a link that leaves the site", link("../docs/GUIDE.md"), "leaves site/"),
        ("a root-relative link", link("/agent-utils/other.html"), "root-relative"),
        ("a fragment missing on this page", link("#nowhere"), "has no id 'nowhere'"),
        ("a fragment missing on another page", link("other.html#nowhere"), "has no id"),
        ("an empty fragment link", link("#"), "goes nowhere"),
        ("a javascript: link", link("javascript:void(0)"), "unsupported URL scheme"),
        ("a plain-http link", link("http://example.org/"), "https://"),
        ("an absolute link to this site", link(PAGES_URL + "/other.html"), "relatively"),
        ("an external script",
         page_with(head_extra='<script src="https://cdn.example/x.js"></script>\n'),
         "from another origin"),
        ("an external stylesheet",
         page_with(head_extra='<link rel="stylesheet" href="https://fonts.example/f.css">\n'),
         "from another origin"),
        ("a protocol-relative frame", body('<iframe src="//video.example/e" title="v"></iframe>'),
         "from another origin"),
        ("an external image", body('<img src="https://img.example/a.png" alt="a">'),
         "from another origin"),
        ("an external srcset candidate",
         body('<img src="mark.svg" srcset="mark.svg 1x, https://img.example/b.png 2x" alt="a">'),
         "from another origin"),
        ("a data: image", body('<img src="data:image/png;base64,AAAA" alt="a">'),
         "keep assets as files"),
        ("a missing image", body('<img src="missing.png" alt="a">'), "does not resolve"),
        ("an image without alt", body('<img src="mark.svg">'), "without alt"),
        ("an unnamed inline svg", body('<svg viewBox="0 0 1 1"><rect width="1" height="1"/></svg>'),
         "needs aria-hidden"),
        ("an svg labelled by a missing id",
         body('<svg role="img" aria-labelledby="nope"><title>t</title></svg>'), "missing id"),
        ("an inline style attribute", body('<p style="color:red">x</p>'), "inline style"),
        ("an inline event handler", body('<p onclick="go()">x</p>'), "event handler"),
        ("a <style> element", page_with(head_extra="<style>p{}</style>\n"), "<style> element"),
        ("a canonical link",
         page_with(head_extra='<link rel="canonical" href="index.html">\n'), "canonical"),
        ("a skipped heading level", body("<h2>a</h2><h4>b</h4>"), "skipping a level"),
        ("a second h1", body("<h1>again</h1>"), "exactly one <h1>"),
        ("a repository link to an untracked path",
         link(REPO_URL + "/blob/main/docs/NOPE.md"), "not a tracked file"),
        ("a repository link to a tracked symlink",
         link(REPO_URL + "/blob/main/docs/LINK.md"), "tracked symlink"),
        ("a blob link to a directory", link(REPO_URL + "/blob/main/docs"), "is a directory"),
        ("a tree link to a file", link(REPO_URL + "/tree/main/README.md"), "is a file"),
        ("a tree link to a missing directory", link(REPO_URL + "/tree/main/nothing"),
         "no tracked directory"),
        ("a repository link off main", link(REPO_URL + "/blob/dev/README.md"), "blob/main"),
        ("a Markdown fragment with no heading",
         link(REPO_URL + "/blob/main/docs/GUIDE.md#phase-9"), "no heading or anchor"),
        ("a fragment naming a heading inside a fence",
         link(REPO_URL + "/blob/main/docs/GUIDE.md#inside-a-fence"), "no heading or anchor"),
        ("a non-line fragment on a source file",
         link(REPO_URL + "/blob/main/docs/tool.py#main"), "line anchor"),
        # Page shape and landmarks.
        ("a first link that is not the skip link",
         edited(lambda t: t.replace('<a class="skip" href="#main">Skip to content</a>\n', "")
                .replace("<header>", '<header><a href="other.html">Other</a>')),
         "first link on the page must be the skip link"),
        ("a page without <header>",
         edited(lambda t: t.replace("<header>", "<div>").replace("</header>", "</div>")),
         "no <header> landmark"),
        ("a page without <nav>",
         edited(lambda t: t.replace("<nav>", "<div>").replace("</nav>", "</div>")),
         "no <nav> landmark"),
        ("a page without <footer>",
         edited(lambda t: t.replace("<footer>", "<div>").replace("</footer>", "</div>")),
         "no <footer> landmark"),
        ("a second <main>", body('<main id="again"><p>x</p></main>'), "exactly one <main>"),
        ("a <main> without an id",
         edited(lambda t: t.replace('<main id="main">', "<main>")), "<main> has no id"),
        ("a missing viewport",
         edited(lambda t: t.replace('<meta name="viewport" content="width=device-width, '
                                    'initial-scale=1">\n', "")), "viewport"),
        ("an <h2> before the <h1>",
         edited(lambda t: t.replace("<h1>Fixture</h1>", "<h2>First</h2>\n<h1>Fixture</h1>")),
         "the first heading must be the <h1>"),
        ("an empty id", body('<p id="">x</p>'), "empty id"),
        ("a <base> element", page_with(head_extra='<base href="https://example.org/">\n'),
         "<base>"),
        # The page policy.
        ("a page with no policy <meta> at all",
         edited(lambda t: t.replace(csp_line(t), "")), "no Content-Security-Policy <meta>"),
        ("a policy <meta> after the stylesheet",
         edited(lambda t: t.replace(csp_line(t), "").replace(
             '<link rel="stylesheet" href="style.css">\n',
             '<link rel="stylesheet" href="style.css">\n' + csp_line(t))),
         "must come before the first script or stylesheet"),
        ("a policy <meta> after an inline script",
         edited(lambda t: t.replace(csp_line(t), "").replace(
             "</script>\n", "</script>\n" + csp_line(t))),
         "must come before the first script or stylesheet"),
        ("two policy <meta> elements",
         edited(lambda t: t.replace(csp_line(t), csp_line(t) * 2)),
         "more than one Content-Security-Policy"),
        # Links.
        ("a fragment on a file that is not a page", link("style.css#x"), "not a page"),
        ("a page link to the unpublished site README", link("README.md"), "is not published"),
        ("a repository link with a query string",
         link(REPO_URL + "/blob/main/README.md?plain=1"), "drop the query string"),
        ("a repository directory link with a fragment",
         link(REPO_URL + "/tree/main/docs#x"), "has no #x"),
        ("a blob link that names a directory with a slash",
         link(REPO_URL + "/blob/main/docs/"), "must name a file"),
        ("a fragment on a tracked Markdown file that is not in the checkout",
         link(REPO_URL + "/blob/main/README.md#x"), "cannot read"),
        ("a fragment naming a heading inside a quoted fence",
         link(REPO_URL + "/blob/main/docs/GUIDE.md#inside-a-quoted-fence"),
         "no heading or anchor"),
        # Files.
        ("a symlink under site/", symlink, "symlinks do not survive"),
        ("a published file that git does not track", untracked, "not tracked by git"),
    ]

    def css(text: str) -> Callable[[Path, Repo], Repo | None]:
        def mutate(site: Path, repo: Repo) -> Repo | None:
            (site / "style.css").write_text(text, encoding="utf-8")
            return None
        return mutate

    def add_file(name: str, size: int) -> Callable[[Path, Repo], Repo | None]:
        def mutate(site: Path, repo: Repo) -> Repo | None:
            (site / name).parent.mkdir(parents=True, exist_ok=True)
            with open(site / name, "wb") as handle:
                handle.truncate(size)
            return None
        return mutate

    def with_demo(markup: str) -> Callable[[Path, Repo], Repo | None]:
        """A recorder under site/demos/, plus `markup` in the index page's body."""
        def mutate(site: Path, repo: Repo) -> Repo | None:
            add_file("demos/clips/record.py", 10)(site, repo)
            # other.html links back to index.html#part, so the page keeps that heading.
            (site / "index.html").write_text(
                _page(body=markup + '\n<h2 id="part">Part</h2>'), encoding="utf-8"
            )
            return None
        return mutate

    def remove(name: str) -> Callable[[Path, Repo], Repo | None]:
        def mutate(site: Path, repo: Repo) -> Repo | None:
            (site / name).unlink()
            return None
        return mutate

    def svg(text: str) -> Callable[[Path, Repo], Repo | None]:
        def mutate(site: Path, repo: Repo) -> Repo | None:
            (site / "mark.svg").write_text(text, encoding="utf-8")
            return None
        return mutate

    cases += [
        ("a stylesheet url() from another origin",
         css(".a{background:url('https://cdn.example/a.png')}"), "everything loads from"),
        ("a stylesheet @import from another origin",
         css("@import 'https://fonts.example/f.css';"), "everything loads from"),
        ("a stylesheet url() to a missing file", css(".a{background:url(nope.png)}"),
         "does not resolve"),
        ("an SVG that references another origin",
         svg('<svg><image href="https://img.example/a.png"/></svg>'), "everything loads from"),
        ("a GIF over 3 MB", add_file("demo.gif", GIF_LIMIT + 1), "exceeds"),
        ("an image over its cap", add_file("shot.png", IMAGE_LIMIT + 1), "exceeds"),
        ("a text asset over its cap", add_file("big.css", TEXT_LIMIT + 1), "exceeds"),
        ("an unknown file type", add_file("tool.php", 10), "not a known website file type"),
        ("a missing .nojekyll", remove(".nojekyll"), ".nojekyll is missing"),
        ("a missing index.html", remove("index.html"), "index.html is missing"),
        ("a page link into the unpublished demos/",
         with_demo('<p><a href="demos/clips/record.py">x</a></p>'), "is not published"),
        ("an image loaded from the unpublished demos/",
         with_demo('<img src="./demos/clips/record.py" alt="a">'), "is not published"),
        ("an unknown file type beside, not inside, demos/",
         add_file("demos-old/record.py", 10), "not a known website file type"),
    ]

    for why, mutate, needle in cases:
        problems = run(mutate)
        expect(
            any(needle in problem for problem in problems),
            f"{why} must fail with {needle!r}; got {problems}",
        )

    # A GIF exactly at the cap is allowed: the cap is inclusive, so this is the boundary control.
    at_cap = run(add_file("demo.gif", GIF_LIMIT))
    expect(at_cap == [], f"a GIF of exactly {GIF_LIMIT:,} bytes must pass, got {at_cap}")

    # Headings GitHub anchors inside a block quote or a list item are valid link targets.
    quoted = run(body(f'<p><a href="{REPO_URL}/blob/main/docs/GUIDE.md#quoted-heading">x</a></p>'
                      '<h2 id="part">Part</h2>'))
    expect(quoted == [], f"a link to a quoted heading must pass, got {quoted}")

    # The recorder's own files are not website files: no type or size rule applies to them.
    demo_source = run(with_demo("<p>x</p>"))
    expect(demo_source == [], f"files under demos/ are not website files, got {demo_source}")

    # --stage publishes exactly what was checked: everything but demos/, nothing extra.
    with tempfile.TemporaryDirectory(prefix="check-site-self-test-") as raw:
        root = Path(raw)
        repo = _fixture(root)
        site = root / SITE
        with_demo("<p>x</p>")(site, repo)
        staged_dir = root / "staged"
        count = stage(site, staged_dir)
        staged = sorted(path.relative_to(staged_dir).as_posix()
                        for path in staged_dir.rglob("*") if path.is_file())
        want = [".nojekyll", "index.html", "mark.svg", "other.html", "style.css"]  # no README.md
        expect(staged == want and count == len(want),
               f"--stage must copy exactly {want}, copied {staged} ({count})")
        refused = False
        try:
            stage(site, staged_dir)
        except SystemExit:
            refused = True
        expect(refused, "--stage into a directory that already holds files must be refused")

    for failure in failures:
        print(f"FAIL  {failure}", file=sys.stderr)
    if failures:
        print(f"\n{len(failures)} self-test failure(s) of {controls} controls", file=sys.stderr)
        return 1
    print(f"check-site --self-test: PASSED ({controls} controls)")
    return 0


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--self-test", action="store_true",
        help="check the checker: a valid fixture site must pass and a variant breaking each "
        "rule must fail with that rule's message; then exit",
    )
    parser.add_argument(
        "--stage",
        type=Path,
        metavar="DIR",
        help="after a passing check, copy exactly the published files (everything under site/ "
        "except site/demos/ and site/README.md) into DIR, which must be absent or empty; the "
        "Pages workflow uploads DIR. Nothing is copied when the check fails",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)
    if args.self_test:
        return self_test()

    report = check_site(REPO_ROOT / SITE, repository())
    if report.problems:
        print("check-site: FAIL", file=sys.stderr)
        for problem in report.problems:
            print(f"  {problem}", file=sys.stderr)
        print(f"\n{len(report.problems)} problem(s); see `python3 scripts/check_site.py --help`",
              file=sys.stderr)
        return 1
    print(
        f"check-site: ok — {report.pages} pages, {report.files} files, "
        f"{report.local_links} local links, {report.repo_links} repository links"
    )
    if args.stage is not None:
        copied = stage(REPO_ROOT / SITE, args.stage)
        print(f"check-site: staged {copied} published files in {args.stage}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
