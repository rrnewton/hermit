# vibe-talk demo clips: a mock Google Chat space, recorded

The website's vibe-talk page ([`site/vibe-talk.html`](../../vibe-talk.html)) shows five short
clips of vibe-talk in use. This directory is how they were made, so anyone can make them again
after the page changes. It is repository source, not part of the website:
`scripts/check_site.py --stage` leaves `site/demos/` out of what GitHub Pages publishes, and the
checker refuses a page link into it.

Nothing here contacts a chat or model vendor, and nothing needs a credential. Everything runs on
127.0.0.1:

* `gchat_bridge_mock.py` stands in for a Google Chat bridge. It serves one fictional Google Chat
  space through the Discord-compatible bridge contract that vibe-talk reads
  (`thread_api = "bridge"`, `markup = "google-chat"`). It also runs a canned Messages API that
  returns hand-written summaries and thread names. Standard library only.
* The **real** vibe-talk server, a release build from this repository, is started with
  `vibe-talk.toml.template` filled in, pointing at both mocks. The page in every clip is the real
  page, served by that binary.
* `record_demo.py` drives headless Chromium through five scripted clips with Playwright. The
  DevTools screencast captures every frame the compositor draws, with its timestamp. ffmpeg
  then turns each clip into a GIF and a poster PNG.

vibe-talk cannot tell this mock from a real bridge. The mock serves `GET/POST
/channels/{id}/messages`, the bridge `timeline` views (`main`, `threads`, `flat`, `thread`),
thread message lookup and thread posts, and `GET /users/@me`. Message ids are real snowflakes, so
ordering and times behave as they do upstream. Posts from the page reach the mock. A post that
answers the scripted question makes the "agents" answer a few seconds later. Each new message is
followed by a content-free change hint (`POST /api/v1/live/hints`), so the server reads the bridge
straight away instead of waiting for its next poll. The page's own live stream then delivers it,
as it would in a deployment.

## Re-recording, end to end

Run these from the repository root.

```sh
# 1. Build vibe-talk. web/ is compiled into the binary, so rebuild after any page change.
cargo build --release --manifest-path vibe-talk/Cargo.toml

# 2. One-time tool setup, if not already present. ffmpeg must also be on PATH (or pass --ffmpeg).
python3 -m pip install --user playwright
python3 -m playwright install chromium

# 3. Record all five clips, encode them, and copy the GIFs and posters into site/media/vibe-talk/.
#    About two minutes.
python3 site/demos/vibe-talk/record_demo.py --publish

# 4. Check the website, then look at every clip before committing it. A clip with a new name
#    must be `git add`-ed first: the checker refuses a website file that git does not track.
python3 scripts/check_site.py
```

Other useful forms:

```sh
# Only some clips:
python3 site/demos/vibe-talk/record_demo.py --only reply-from-phone,desktop-threads --publish

# Re-encode from frames already recorded, to try other encoder settings:
python3 site/demos/vibe-talk/record_demo.py --encode-only --fps 10 --desktop-width 800
```

`--vibe-talk` defaults to `$VIBE_TALK_BIN`, then to `vibe-talk/target/release/vibe-talk`. Output
goes to `--out`, by default `$XDG_CACHE_HOME/agent-utils/vibe-talk-demo` (else
`~/.cache/agent-utils/vibe-talk-demo`), never into the checkout. With `--headed` you can watch the
browser. `python3 site/demos/vibe-talk/record_demo.py --help` describes every option.

If you change `--phone-width` or `--desktop-width`, update each clip's `width` and `height` in
`site/vibe-talk.html` to match, or the page will draw the clips at the wrong shape.

Nothing in `make validate` records clips: that needs Chromium, ffmpeg and a release build.
Validation does type-check both Python files (`make mypy`), and `scripts/check_site.py` checks the
published GIFs and posters, capping a GIF at 3 MB.

## Files

| Path | What it is |
|---|---|
| `gchat_bridge_mock.py` | The mock bridge, the scripted conversation, and the canned Messages API. It can also run on its own (`--help`). |
| `record_demo.py` | The recorder. It starts both servers, films the clips, encodes the GIFs and posters, stops everything it started, and with `--publish` copies the results into `site/media/vibe-talk/`. |
| `vibe-talk.toml.template` | The vibe-talk configuration for one run. The recorder fills in ports, throwaway tokens and the run's directory. |
| `fonts.conf` | A fontconfig file used only by the recorder's Chromium. It resolves `system-ui` to Droid Sans, the font early Android phones drew their interface in, instead of the recording host's default sans-serif. The page's CSS is not changed. |
| `README.md` | This file. |

What one run leaves under `--out`:

| Path | What it is |
|---|---|
| `<clip>.gif` | The animated GIFs. |
| `<clip>-poster.png` | A still from each clip, at the GIF's size, taken at the frame the clip script marks as representative. |
| `clips.json` | Each GIF's path, byte size, dimensions, duration and number of captured frames. |
| `frames/<clip>/` | Raw screencast PNGs, `timeline.json` (frame timestamps and marks) and `sequence/` (symlinks resampled to the GIF frame rate). About 100 MB in total. |
| `run/` | That run's vibe-talk configuration, the SQLite store, and both server logs. |

## Ports

All three listen on 127.0.0.1 only and are checked free before anything starts:

| Port | Process |
|---|---|
| 19310 (`--port-base`) | vibe-talk |
| 19311 | mock bridge (`/api/v10/...`) and its control plane (`/_demo/reset`, `/_demo/cue/{name}`, `/_demo/state`) |
| 19312 | canned Messages API (`POST /v1/messages`) |

Use `--port-base` to move all three together.

## The clips

| GIF | Size | Duration | What it shows |
|---|---|---|---|
| `channel-summaries.gif` | 412x915 | 12.8 s | Long agent messages folded, then the **Summaries** chip turns each into one sentence. |
| `long-report.gif` | 412x915 | 9.6 s | A summarised report opened with one tap: headings, bullets, a table, a numbered list, labelled links. |
| `live-message.gif` | 412x915 | 12.4 s | Two agent messages arrive while the page is open. **N new** jumps to them. |
| `reply-from-phone.gif` | 412x915 | 18.3 s | Reply to an approval request. The answer and a confirmation arrive. |
| `desktop-threads.gif` | 800x500 | 19.1 s | Desk layout: Main beside named thread cards. Open a thread, reply in it, and the agent answers. |

Sizes and durations are those of the committed clips; a new recording differs slightly. Every
clip begins from a fresh bridge (`/_demo/reset`), a fresh browser context and the dark scheme, so
each one stands alone and `--only` reproduces it. Phone clips use 412x915 at device scale factor 2.
Desktop clips use 1000x625 at scale 2, scaled to 800 px wide.

## The scripted conversation

All of it is fictional: an owner ("Sam") supervising a coordinator and three worker agents on a
made-up project, `acme-widgets`. The agents post through one shared app account and name
themselves in a `[tag]`, which vibe-talk lifts into the row header. The messages mention
`wrkslots`, `agentctl` and `make validate`. Links point at `example.com`. Times are relative to
the moment of the reset, so the clips show the date and wall-clock time of the recording, in US
Eastern time.

Cues, added while recording: `worker-report` (two messages, 6 s apart), plus two triggered by
posts from the page. A reply to the approval request triggers `approval-landed`. A post into the
CSV report's thread triggers `report-ack`.

## Encoding

The screencast sends frames only when something changes, up to 60 a second. `record_demo.py`
resamples them onto a constant 10 fps clock, which matches GIF's centisecond delays exactly
(10 cs per frame). ffmpeg builds one palette per clip (`palettegen=stats_mode=full`, 256 colours)
and applies it with `paletteuse=dither=none:diff_mode=rectangle`. Flat interface colour and
antialiased text survive 256 colours cleanly, and undithered frames differ only where the page
changed. The five committed GIFs total about 3.6 MB; the largest is 1.4 MB.

## Fragile, or not shown

* **Selectors.** The clip scripts use the page's own ids and attributes: `#view-switch`,
  `#summarise`, `#scroll-area`, `.reply-button`, `#reply-text`, `#reply-send`, `#jump-newest`,
  `#column-toggle`, `#side-cards`, `#side-compose-text`, `#side-send`, `#side-scroll`, and the
  `data-search`, `data-summarised` and `data-collapsed` row attributes. A page rework that renames
  them breaks a clip. It fails loudly, on a 15-20 s wait; it never records the wrong screen
  silently.
* **Timing.** vibe-talk spaces hinted reads at least 5 s apart, so scripted messages are 6 s
  apart (`worker-report`) or 5.3 s apart (`approval-landed`). Closer spacing makes the second
  message arrive late. vibe-talk's own poll runs every 120 s. If a periodic poll lands just
  before a cue, delivery can slip by up to 5 s, which lengthens that clip.
* **The thread picker drop-down is not filmed.** It is a native `<select>`, and headless
  Chromium never draws native pop-ups into the page. The desktop clip shows threads through the
  two-column view's thread cards instead. Search is not filmed either.
* **No emoji.** The recording host's Chromium had no colour emoji font, so the mock sends no
  reactions (`"reactions": []`). They would be drawn as empty boxes.
* **Fonts.** `fonts.conf` needs Droid Sans at the fontconfig default path. Without it, fontconfig
  falls back to the host's sans-serif and the clips still record, in a wider font. With Droid
  Sans, the narrow "Show read" button label nearly touches its button edges.
* **Recording aids.** The recorder draws a white ripple where a tap or click lands and, on the
  desktop, an arrow pointer, because headless Chromium draws neither. These are injected by the
  recorder, not part of vibe-talk.
* **Frame size.** The headless screencast delivers frames at CSS-pixel size, scaled down from the
  device-pixel rendering. The phone GIFs are therefore 412 px wide, 1:1 with CSS pixels. A wider
  phone GIF would be upscaled and soft. Use `page.screenshot` in a loop (about 10-15 frames a
  second at 824x1830) if a larger GIF is ever needed.
* **Voice is not shown.** `/voice` call states depend on sound and a vendor connection. The clips
  stay on the channel view.
* **What the mock does not implement.** Forward cursors (`next_after`; vibe-talk falls back to
  its generic cursor), channel registration and discovery, upstream read marks, `recent_view =
  "flat"`, and reactions.

## What the clips do and do not demonstrate

The page, the server, rich headers, Markdown rendering, threads, posting, reply-marks-done, live
delivery and summaries are all the real vibe-talk code paths. The chat service and the model are
mocks with canned text. Per vibe-talk's own status table, the Messages API summariser has been
tested only against a local mock, never against a live model provider. Posting tests use local
fakes. A compatible Google Chat bridge has been exercised with real read-only traffic, and no such
bridge ships in this repository. The website says these clips run against local mocks.
