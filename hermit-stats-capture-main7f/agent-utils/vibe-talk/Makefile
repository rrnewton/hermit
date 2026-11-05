# vibe-talk's own validation, runnable from this directory alone.
#
# WHY THIS EXISTS SEPARATELY. vibe-talk is a service, not one of the paired command-line tools, and
# it lives outside the `rs/` workspace with its own lockfile. The repository's `make check` and
# `make test` do not see it at all. The repository-wide `make validate` does -- it selects the
# vibe-talk group when vibe-talk changed -- but it is run from the root and reasons about the whole
# tree, and the loop you actually work in is "I changed one file in here, is this tool still good?"
#
# So: `make validate` at the root is the repository's boxed gate, and `make validate` HERE is this
# tool's lightweight unboxed gate. The root includes `tests.dag.yaml` directly so vibe-talk shares
# one scheduler and browser cap with the other repository fragments.

.PHONY: validate validate-boxed dag-check fmt check scripts-check contract contract-check typecheck test page pwa-installability enter-to-send-browser offline-cache-browser reactions-browser rich-headers-browser read-aloud-browser post-confirm-browser voice-call-drops-browser send-order-browser view-switch-browser read-modes-browser keyboard-browser two-column-browser bookmark-browser reply-continuity-browser reply-in-column-browser voice-read-acceptance-self-test android-speech-self-test live-hint-adapter-self-test container-smoke filter-place-browser search-dates-browser voice-composer-browser thread-button-browser done-feedback-browser elision-browser clean thread-card-focus-browser scroll-stability-browser

# The whole gate for this tool, cheapest and most likely to fail first.
#
# UNBOXED, and deliberately the default: it needs Rust, Node, and the repository's Python typing
# tools, but no cgroup delegation. `validate-boxed` is the bounded version where cgroups exist.
validate: fmt check scripts-check contract-check typecheck test dag-check voice-read-acceptance-self-test android-speech-self-test live-hint-adapter-self-test

# The complete portable suite, run through dagrun: per-step wall and CPU timeouts, memory caps,
# and process-tree teardown inside nested cgroups. Container-runtime and alternate-toolchain
# checks remain focused CI signals because they require a different execution environment.
#
# PREFER THIS LOCALLY. It is the only path that bounds what the screenshot harness starts -- the
# teardown is setsid-proof, so a browser or a throwaway server is reaped with the step whether or
# not the script's own trap fired. One that was not once ran for six days holding a port.
#
# It also adds PWA installability, offline-cache, and screenshot browser suites, which plain
# `validate` leaves out because they need a browser.
validate-boxed:
	cd .. && ./common/bin/dagrun run --dag vibe-talk/tests.dag.yaml

# `./common/bin/dagrun`, not a bare `dagrun` and not `./bin/dagrun`. This repository ships the tool
# it uses, so depending on the caller having installed one would fail for a reason that has nothing
# to do with this tool -- and `./bin/` is GITIGNORED, existing only after `./setup` has run, so
# pointing at it failed on a fresh clone for a reason that had nothing to do with it either.
#
# The graph itself, checked without running anything. Offline and instant.
#
# This used to prove almost nothing, and said so: `dagrun list` parsed the document and printed it,
# accepting an unknown field, a duplicate step tag, a missing dependency and a cycle -- the last of
# which CRASHED the runner rather than being refused. `#96 dagrun-loader-validation` moved all of
# those into the loader, so every inspection subcommand gets them: this now refuses a closed-schema
# violation, a duplicate tag, a missing dependency, a named cycle, and a demand above its cap, at
# exit 2, having executed nothing.
dag-check:
	cd .. && ./common/bin/dagrun list --dag vibe-talk/tests.dag.yaml >/dev/null

fmt:
	cargo fmt --all -- --check

check:
	cargo clippy --all-targets --locked -- -D warnings

# These are deployed operator tools, not incidental repository helpers. Keep their strict typing
# in this tool's own gate so changing one does not need the unrelated root workspaces merely to
# reach a mypy invocation.
scripts-check:
	cd .. && python3 -m mypy --config-file py/pyproject.toml --strict \
		--disallow-any-explicit --disallow-any-decorated vibe-talk
	cd .. && python3 scripts/check_no_any.py vibe-talk

# THE WIRE CONTRACT. `src/contract.rs` names the JSON the server and the page exchange; the files
# under contract/ and web/contract.* are generated from it and checked in, so a change to a wire
# type arrives as a reviewable diff. `contract` regenerates all of them: the schema and samples
# from the Rust types, then the browser's validators and declarations from the schema, then every
# contract test against the result. The browser-validator test is skipped while rewriting: it
# would judge the OLD validators against the NEW samples, and stop the recipe before regenerating.
# `contract-check` fails, naming the stale file, when the browser files no longer match the
# schema; `cargo test --test contract` does the same for the schema against the Rust types.
#
# Both, and `typecheck`, need the generator's pinned tools: `npm ci --prefix contract` once.
# Serving the page never does -- web/contract.js is plain checked-in JavaScript.
contract:
	VIBE_TALK_UPDATE_CONTRACT=1 cargo test --locked --test contract -- --skip the_browser_validators
	node contract/generate.mjs
	cargo test --locked --test contract

contract-check:
	node contract/generate.mjs --check

# TypeScript checks web/voice.js as JavaScript (checkJs, noEmit) against the generated
# declarations. Nothing is compiled or bundled; see tsconfig.json.
typecheck:
	contract/node_modules/.bin/tsc -p tsconfig.json

# `cargo test` runs BOTH served pages' suites through `node --test` as well as the Rust tests, so
# this is not only the Rust half. Those suites FAIL rather than skip when node is missing, which is
# what stops a page regression from passing silently on a machine without a node toolchain.
test:
	cargo test --locked

# The page suites on their own, for the edit-run loop. Seconds rather than a full cargo test.
#
# EVERY suite, by glob, not a named file: this page serves two -- `/` and `/voice` -- and naming one
# of them is how the second comes to exist without ever being run.
page:
	node --test tests/js/*.test.mjs

# A real current Chromium, speaking CDP. This validates the installability decision the DOM-only
# page suite cannot make. It uses a loopback secure context by default; set VIBE_TALK_CHROMIUM to
# inspect with a particular Chrome executable, or pass --url directly to the script for a deployed
# server. It deliberately states that this is not physical-device launcher proof.
pwa-installability:
	python3 tests/pwa_installability.py

# `#225 enter-to-send` in a real Chromium at a desk's size: real keys in the reply box. By default
# Shift+Enter is a new line and Enter posts the reply; with "Enter to send" off in Settings (its hint
# naming the key that sends, kept across a real reload), Enter is a new line and Ctrl+Enter posts.
# Pass WEB_ROOT=dir to run it against another copy of the page.
enter-to-send-browser:
	python3 tests/enter_to_send_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT))

# `#18 offline-message-cache` in a real Chromium at a phone's size: localStorage across a real
# reload, a real fetch failure, the Cache Storage and service-worker registries, and the pill's
# layout against the view tabs -- what the DOM-only suite cannot observe. Loopback fake API only.
# Pass SCREENSHOTS=dir to keep one PNG per step.
offline-cache-browser:
	python3 tests/offline_cache_browser.py $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#219 emoji-reactions` in a real Chromium: every reaction strip laid out under its row's text,
# clear of the row's controls and legible, at 412px, 360px and a desk, dark and light. Pass
# SCREENSHOTS=dir to keep PNGs.
reactions-browser:
	python3 tests/reactions_browser.py $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#242 rich-headers` in a real Chromium: each agent's tag, title and time drawn whole in the row's
# header where it fits and as its first line where it does not, the words they restate hidden and
# none lost, at 412px and a desk (and two columns), and the Settings switch drawing every first line
# as written, kept across a reload. Pass SCREENSHOTS=dir to keep PNGs.
rich-headers-browser:
	python3 tests/rich_headers_browser.py $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#7 read-aloud-latency` in a real Chromium: tap to audible against audio streamed at speaking
# pace, a switch and a stop closing the response, and only a read heard to its end archived. Its
# negative control is the same tap through `<audio src>`, which must take seconds. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep PNGs.
read-aloud-browser:
	python3 tests/read_aloud_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#49 post-confirmation-card-hold` in a real Chromium: a real touch on Send the moment a first
# post-confirmation card appears commits nothing, and Send enables itself after the hold. Pass
# WEB_ROOT=dir to run it against another copy of the page.
post-confirm-browser:
	python3 tests/post_confirm_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT))

# `#268 voice-call-drops` in a real Chromium: a spoken call on the fake microphone replaying the
# voice bridge's event order -- no microphone frame inside the greeting's hand-off, a "Yes." whose
# final transcript arrives after the agent's reply committing the card once, the page's notice
# waiting for that turn, the server's close said and recorded, and a hang-up sending one goodbye.
# Pass WEB_ROOT=dir to run it against another copy of the page.
voice-call-drops-browser:
	python3 tests/voice_call_drops_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT))

# `#277 send-order` in a real Chromium against the real server binary, which posts through its real
# provider client to a mock Google Chat bridge on loopback that fails A at once: B waits behind A,
# saying so, and the bridge never sees it; A goes again under the same nonce and lands, then B;
# across a real reload a message still waits behind the one before it, sending nothing, until Retry
# lands both in order; Dismiss on a failing message lets the next go; a slow bridge is given one post
# at a time, in order; and a post whose caller hangs up mid-post still lands before the next one.
# Builds the release binary first, because web/ is compiled into it. Pass SCREENSHOTS=dir to keep a
# PNG of each waiting state.
send-order-browser:
	cargo build --release --locked
	python3 tests/send_order_browser.py $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#241 voice-composer` in a real Chromium at 412x915 and 1280x800: the call's message box and the
# view switch on screen and uncovered while the box has the focus -- at idle, in a typed
# conversation, in a call, and on the phone with the on-screen keyboard's share of the screen taken
# away -- focus and typing opening nothing, and the first send opening one typed conversation. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep PNGs.
voice-composer-browser:
	python3 tests/voice_composer_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#220 view-switch-instant` in a real Chromium: All to Main and back with every timeline request
# intercepted -- no read, never an empty list or the loading line, the reader's message kept -- All
# never read drawn at once from held rows and read once behind them, and, paged, Main drawn from All
# keeping its way back and All walked back to the reader's message before it is drawn. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep PNGs.
view-switch-browser:
	python3 tests/view_switch_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#221 read-modes` in a real Chromium at 412x915 and 1280x800: entering a thread lands on its first
# unread message, unfolded, under the pill, in each read mode, and Collapse read's "… N read
# messages …" line is a 44px target a real tap opens without moving the reader; then, `#222
# unread-replies`, Main under Hide read keeps the read question for its unread replies, says "4 unread"
# beside "52 replies", and a real tap on the count lands on the first of them. Pass WEB_ROOT=dir to
# run it against another copy of the page, and SCREENSHOTS=dir to keep a PNG of each step.
read-modes-browser:
	python3 tests/read_modes_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#224 keyboard-shortcuts` in a real Chromium at 1280x800, the keys pressed through CDP
# Input.dispatchKeyEvent so the browser's own defaults run: PgUp, PgDn, Space, Home and End move the
# list on screen with the focus where the page put it on load and after a view switch; "/" and Ctrl+S
# open the search (the browser's Save page refused); "," and Ctrl+, open Settings and Escape comes
# back; in the message box PgUp pages the list and Ctrl+Home/End jump it; then the keymap: j, k and the
# arrows move a visible focus ring, e marks Done and moves on, PgDn brings the ring to the page, Enter
# folds, and -> enters a thread that <- and Esc leave; Ctrl+K and a channel chosen give the keys back to
# the list, and Esc leaves the picker. Pass WEB_ROOT=dir to run it against another copy of the page,
# and SCREENSHOTS=dir to keep a PNG of each step.
keyboard-browser:
	python3 tests/keyboard_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#229 desktop-two-column` in a real Chromium at 1440x900 and 1280x800: the layout button draws Main
# and the right column side by side inside the window, the view picker greyed; the right column's cards
# are one height, newest activity first, the long one cut to three lines; a card's replies button and a
# Main message's Thread(N) select a thread there under its name, each column scrolls alone under the
# wheel, the X goes back to the cards, and a window narrowed to 900px is one column in its old view, two
# again when wide. Pass WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to
# keep the list and thread states as PNGs.
two-column-browser:
	python3 tests/two_column_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#277 thread-card-focus` in a real Chromium, at 1280x800 over the right column's cards with a mouse and
# the keys, and on a 412x915 phone over the Threads list with a finger: every card contracted, the one
# under the mouse too; a click or a tap highlights one, shown whole and not entered, and every other
# card's edge and every other link turns the page's grey — by the colour Chromium computes and by the
# pixels a screenshot holds there — the highlighted card's staying in its colour; its replies button
# enters its thread, <- or Back comes out to it still highlighted, Esc puts the highlight out, and -> enters
# the card the keys are on. Pass WEB_ROOT=dir to run it against another copy of the page, and
# SCREENSHOTS=dir to keep a PNG of each state.
thread-card-focus-browser:
	python3 tests/thread_card_focus_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#280 scroll-stability` in a real Chromium at 1280x800 in two columns (mouse) and 412x915 in one (touch):
# the reader scrolled up in Main stays on the same row, within a pixel, while a message and a reply arrive
# live, a refresh redraws the list with a message above them grown, a read sent while they were on the
# newest line lands after they scrolled up (the owner's jump), summaries arrive, and the focus moves, and
# at the desk while the mouse crosses the cards to the dock or the top bar and rests, and ] is pressed on
# its own, each sampled every frame for 2.5 s; a reader on the newest line follows a live message to the foot; the list of threads and a thread open at
# their foot and stay there, older replies coming in under a reader at a thread's head; after a
# reload the read behind the saved rows leaves the reader where they scrolled; and, last, a card
# highlighted by a click, the mouse going on over another card to the dock, leaves Main still. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep a PNG of each stage.
scroll-stability-browser:
	python3 tests/scroll_stability_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#240 reading-bookmark` in a real Chromium at 1280x800 (mouse and keys) and 412x915 (touch): the
# bookmark set on a message (Shift+B, or the row's menu) is a line in the accent drawn in the room
# between that row and the one above; scrolled away, the Bookmark chip points to it, directly right of
# Newest on the bottom line; b or the chip puts the line at the top of the list; a reply goes to the
# newest message and b comes back; and a Done on the message under the line slides it to the next, on
# the server too. Pass WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to
# keep a PNG of each step.
bookmark-browser:
	python3 tests/bookmark_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#243 filter-keeps-selection` in a real Chromium at 1280x800 and 412x915, the owner's own sequence:
# search, Pinned, a pin chosen with the mouse or a finger, the filter cleared -- the pin's row stands in
# the channel's list where it stood in the Pinned list, within a pixel, and stays there; then an old pin,
# two pages beyond what is loaded, walked back to under a "Loading earlier messages…" line and put where
# it stood. Pass WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep a
# PNG of each stage.
filter-place-browser:
	python3 tests/filter_place_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#249 search-highlight`, `#250 date-separators` and `#251 author-filter` in a real Chromium at
# 1280x800 (mouse) and 412x915 (touch), over a week of a channel read at a fixed noon: a rule above the
# first message of each day, the bookmark's line nearer the message than its day's; a search's hits lit,
# its gaps said with the days they reach, each line as wide as the list and touching no row; and the
# author filter showing only the owner's prompts with the one chosen left where it stood. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep a PNG of each stage.
search-dates-browser:
	python3 tests/search_dates_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#247 reply-continuity` in a real Chromium at 1280x800 (mouse) and 412x915 (touch), at 80%, 100% and
# 130% text: Reply on a message in the middle of the list, just under where the title bar goes, half
# under the list's top edge, low on the screen, a reply in its gutter, and one taller than the screen.
# The words are the list's size and width, the message's top stays where the row's was (within 1px) or
# at the top, the box comes up directly under it, a low one moves up only as far as shows it, and
# Cancel comes back to the pixel; the reply screen is tinted in the dark scheme and the light. Pass
# WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to keep a PNG of each.
reply-continuity-browser:
	python3 tests/reply_continuity_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#277 reply-in-column` in a real Chromium at 1280x800, in two columns: Reply and Quote reply (words
# dragged over with the mouse) pressed in each column open the reply box at the foot of that column, both
# columns staying where they were to the pixel and the message answered edged and unmoved; Escape, Cancel,
# Send and Enter put the box back and the column where it was, the keys on its list, a sent reply posted
# where the reply screen posts it; a message low in a column moves up only as far as shows it over the
# box; a window narrowed to one column hands an open reply to the reply screen with its words; and one
# column is the reply screen, as it was. Pass WEB_ROOT=dir to run it against another copy of the page,
# and SCREENSHOTS=dir to keep a PNG of each open reply.
reply-in-column-browser:
	python3 tests/reply_in_column_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#252 thread-button-state` in a real Chromium at 412x915 (touch) and 1280x800 (mouse), in the dark
# and light schemes: a new message's Thread tag, whose thread nobody has answered, is drawn as Reply
# is and an answered root's Thread(N) in its thread's colour; the grey tag opens the empty thread
# with its box focused, the first reply is posted into the message's thread, and the tag turns
# coloured; at the desk, the same on the right in two columns. Pass WEB_ROOT=dir to run it against
# another copy of the page, and SCREENSHOTS=dir to keep a PNG of each state.
thread-button-browser:
	python3 tests/thread_button_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#245 done-feedback` in a real Chromium: at 1280x800 in both schemes, a selected unread message ringed
# solid in the accent and a selected read one dashed in grey on the same lit surface, each at 3:1 or
# more against it; e draws the moment on the row it left and lets it go; the Undo chip names the
# message, and an Undo inside the hold takes the ring away at once; reduced motion gets the look with
# no animation; at 412x915 and 1280x800 in both schemes, the pictures; on a 360px and a 412px phone the
# Undo chip says only "Undo", taking no line, and at 1280 it names the message on one line; and at all
# three, walked up from the end of the list through the message box, no chip or status pill meets the
# box or its Send. Pass WEB_ROOT=dir to run it against another copy of the page, and SCREENSHOTS=dir to
# keep a PNG of each step.
done-feedback-browser:
	python3 tests/done_feedback_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# `#244 elision-word-count` in a real Chromium, in seven layouts (phones at 360 and 412px, at 80% and
# 150% text, a desk, the widest reading column, two columns): every folded row's one label claims no
# more than its clamp really hides, by the rects of its characters; Chinese and a bare address are
# counted in characters; a table the clamp draws whole says nothing, and a table over prose, a code
# block and a list of short items say no more than they hide; a rich header's row holds with the switch
# on and off. Pass WEB_ROOT=dir to run
# it against another copy of the page, and SCREENSHOTS=dir to keep a PNG of each layout.
elision-browser:
	python3 tests/elision_browser.py $(if $(WEB_ROOT),--web-root $(WEB_ROOT)) $(if $(SCREENSHOTS),--screenshots $(SCREENSHOTS))

# The live #47 read-aloud-overlong check is opt-in and may consume provider time. Its transcript
# reconciliation, PCM-duration metric, re-answer separation, privacy boundaries, threshold edges,
# interruption rejection, and WAV writer are deterministic offline controls, so keep those in the
# ordinary gate.
voice-read-acceptance-self-test:
	python3 scripts/voice-read-acceptance.py --self-test

# The physical-device run is opt-in, but its signal/language evaluator has offline negative
# controls. Keep those controls in the ordinary gate so a broken harness cannot silently bless a
# later Android recording.
android-speech-self-test:
	python3 scripts/android-device-speech.py --self-test

# A live hint carries no chat content, and its adapter owns a long-running child plus retries.
# Keep the offline HTTP, framing, privacy, and process-cleanup controls in the ordinary gate so a
# deployment does not have to discover a broken supervisor against a real provider event stream.
live-hint-adapter-self-test:
	python3 scripts/live-hint-adapter.py --self-test

# The LAUNCHER -- run.sh, the script the owner types -- as opposed to the image it starts, which
# the `container` CI job covers separately. Not part of `validate` because it needs a container
# engine and a minute of build. Run it before a deploy.
#
# This comment used to claim "CI runs it on every change" while no workflow ran it at all, and
# that claim is why nobody checked: run.sh had shipped inheriting the host's log driver, so on a
# rootless host defaulting to journald `--logs` printed nothing while --status promised retained,
# readable logs. The suite below catches exactly that and had no runner. It has one now, in
# .github/workflows/vibe-talk.yml, so the sentence is true rather than reassuring.
container-smoke:
	./scripts/test-run-sh.sh

clean:
	cargo clean
