#!/usr/bin/env bash
# Build and run the vibe-talk container, with the checks that make a bad start fail HERE instead
# of five minutes later inside a container that came up and quietly does not work.
#
# What it does, in order:
#   1. Refuses to run if a vibe-talk container is already up (see --shutdown / --restart).
#   2. Loads configuration and verifies every REQUIRED variable is actually set, by name.
#   3. Optionally (off by default) makes sure the cloudflared tunnel unit is running.
#   4. Rebuilds the image.
#   5. Removes a leftover STOPPED container, proves the host directory for durable state is
#      writable by the container's own user, then runs the new one detached with it mounted.
#
# DURABLE STATE. The server keeps the /voice transcript, its own read marks and its summary cache
# in a SQLite file. That file must live OUTSIDE the container: this script replaces the container
# on every launch, so anything written into the image's writable layer is destroyed by the next
# deploy, silently. VIBE_TALK_DATA_DIR (default: ${XDG_DATA_HOME:-$HOME/.local/share}/vibe-talk)
# is created 0700 and mounted at /var/lib/vibe-talk. --status reports it. Deleting that directory
# is the purge.
#
# The mount also has to be WRITABLE BY THE CONTAINER'S USER, which is not automatic and is not
# what a bind mount gives you: the image runs as uid 10001, and under rootless podman a directory
# owned by the invoking user appears inside the container as root's. Step 5 therefore maps the
# container's server user back onto this one (--userns=keep-id) and then PROVES the mount is
# writable, with the real image, before launching anything. The server refuses to start when it
# cannot open its store, so without that check a bad mount is a crash loop discovered from logs.
#
# Usage:
#   scripts/run.sh [--shutdown] [--restart] [--logs] [--follow] [--status] [--tunnel-status]
#                  [--smoke-agent [--nonce] [--replay-check]]
#                  [--screenshots [--out DIR] [--theme THEME]]
#                  [--config FILE] [--tunnel|--no-tunnel] [--tag TAG] [--port PORT]
#                  [--restart-policy POLICY] [--build-network MODE] [--dry-run] [--help]
#
# Flags, one action per invocation:
#   --shutdown     Stop the running vibe-talk container and EXIT (it does not go on to rebuild or
#                  relaunch). Use --restart for stop-then-relaunch. Stopping KEEPS the container
#                  and its logs; see --logs.
#   --restart      Stop the running vibe-talk container, then continue with rebuild and relaunch.
#   --logs         Print the container's output SO FAR and exit — a one-shot dump. Works on a
#                  stopped container too, which is the whole point of not using --rm. Reads a
#                  file the container owns, because this script pins the log driver rather than
#                  inheriting it ($VIBE_TALK_LOG_DRIVER; k8s-file under podman, json-file under
#                  docker). Inheriting it is how --logs comes back EMPTY on a host whose default
#                  is journald and whose journal the calling user cannot read.
#   --follow       Stream the container's output live until you interrupt it (Ctrl-C). This is
#                  the "attach" action: a terminal tab running it follows the server exactly as
#                  though you had started the container in the foreground yourself. Nothing is
#                  stopped when you interrupt it; you are only reading.
#   --status       Report what is running, plus config, container name, restart policy, whether
#                  logs are being retained, where durable state is mounted from, and tunnel state;
#                  then exit.
#   --tunnel-status  Report the cloudflared tunnel user unit: whether it is active, since when,
#                  whether it is enabled at boot, and the hostname it serves. STRICTLY READ-ONLY
#                  — it never starts, stops, or enables anything. Exits 0 when the unit is
#                  active, 1 when it is not.
#   --smoke-agent  *** COSTS VENDOR MINUTES. MANUAL ONLY. *** Hold a REAL conversation with the
#                  deployed ElevenLabs voice agent, headless (typed, not spoken), and fail unless
#                  the agent actually CALLED A TOOL on this server during it. This is the check
#                  every other check here cannot make: on 2026-08-19 the owner's session failed
#                  while every server-side check passed, because the agent completed the MCP
#                  handshake and then invoked nothing. Nothing asserted that a tool is called, so
#                  nothing went red.
#                  It is deliberately NOT part of any suite and NOT run by CI: each run opens a
#                  billed conversation. Run it when you want to see what the owner sees. It reads
#                  the channel and never asks the agent to post. The container is only READ from
#                  (its log); nothing is started, stopped, or rebuilt.
#                  Needs the python3 'websockets' package; it says so by name if it is missing.
#
#                  WHAT A RUN COSTS. A PASSING run is ONE short conversation (about 15 seconds of
#                  socket time) and writes NOTHING to the channel. A FAILING run costs roughly
#                  TWICE that: when the agent's reply does not carry the channel message back, the
#                  test automatically escalates — it posts one unique token to the channel through
#                  this server's own write API (never through the agent) and asks again, requiring
#                  the token back verbatim. That second conversation is what separates "the agent
#                  is not really reading" from "the test's own matching was too strict", which are
#                  different problems with opposite fixes. The two are reported as different
#                  results, never as one "smoke failed".
#                  Some runs are REFUSED rather than decided: an empty channel, or a newest message
#                  with nothing distinctive in it, cannot decide the question either way, so the
#                  run stops before opening any conversation and bills nothing.
#   --replay-check Only with --smoke-agent. *** COSTS THREE CONVERSATIONS. *** Asks a DIFFERENT
#                  question from the ordinary run: does the vendor actually act on a replayed
#                  transcript (#46 conversation-replay)? Conversation A states a nonce and its
#                  turns are recorded through this server's own transcript API, exactly as the
#                  /voice page records them; conversation B opens WITH that record and must return
#                  the nonce; conversation C is the CONTROL — same question, no record — and must
#                  NOT be able to answer. Without C the run would prove the agent is fluent rather
#                  than that it remembers, so all three are reported separately.
#                  Refused, billing nothing, when this server has no durable store; refused after
#                  ONE conversation when replay.enabled is false or the transcript came back
#                  empty, because the transcript has to exist before it can be asked for.
#                  "The vendor did not honour it" is a distinct exit code (21), not a generic
#                  failure: it is the single most useful thing this check can tell you, and until
#                  it comes back green the interface must not claim a call was resumed.
#   --nonce        Only with --smoke-agent. Go straight to the token round instead of waiting for
#                  the cheap check to fail — for when you want that evidence on a run that would
#                  otherwise pass cheaply, or when the channel's latest message is too plain to
#                  match on. It always writes one line to the channel. You do not need this to get
#                  the escalation: that happens by itself when the cheap check fails.
#   --screenshots  Photograph the /voice page in all thirty-six states that look different, so an
#                  agent can LOOK at the interface before the owner does. FREE and offline: no vendor
#                  conversation, no microphone, no money. The conversation WebSocket is replaced by
#                  a fake and the microphone is Chromium's built-in fake capture device, so the
#                  live-call, muted and post-call states are reached without ElevenLabs ever being
#                  contacted.
#
#                  The two SUMMARY states cannot use a page-local fake: a summary is a real
#                  conversation with the ElevenLabs agent, so this also starts
#                  vibe-talk-mock-elevenlabs -- this repository's own loopback vendor -- on the two
#                  ports above the server's, and points the throwaway config at it. One state gets
#                  a working summariser; the other wedges the same mock and photographs the red
#                  failed row. Still offline, still free, still nothing billed.
#
#                  Why it exists: the /voice page's own suite drives the real script and asserts
#                  that the right properties sit on the right selectors, but it lays NOTHING out,
#                  so it cannot tell you the page looks right. One photograph of a phone showed
#                  three defects the whole suite had passed over. This is the check that can see.
#
#                  It starts its OWN throwaway server, native (not a container), with
#                  --fake-discord, on port 18091 by default — never 8080, and it refuses to use it.
#                  The mock vendor takes 18092 and 18093 beside it. --port, or the environment
#                  variable VIBE_TALK_SHOTS_PORT when --port is absent, moves all three (P, P+1,
#                  P+2), so several checkouts can photograph at once. It never touches the running
#                  vibe-talk, or its config, or its logs. BOTH processes are stopped again when the
#                  run ends, including on failure.
#
#                  Each capture is checked: the expected state must actually be on screen before
#                  the shutter opens, and the resulting image must not be blank or a single flat
#                  colour. A state that cannot be reached FAILS BY NAME rather than being
#                  photographed approximately.
#
#                  It is opt-in and is in no suite. Needs the python3 'playwright' package and its
#                  Chromium; it says so by name, with the install command, if either is missing.
#                  It rebuilds the binary first, because web/ is compiled INTO it — without that
#                  you photograph the last build's markup and believe it is today's.
#   --out DIR      Only with --screenshots. Where to write the PNGs (default: a timestamped
#                  directory under vibe-talk/debug/screenshots/, which is gitignored). Screenshots
#                  are evidence, never source, and are never committed.
#   --theme THEME  Only with --screenshots. dark (the default), light, or both. DARK IS THE
#                  DEFAULT DELIBERATELY: the owner's phone is dark, and the first run of this
#                  harness captured nothing but light frames because that is the browser
#                  automation default — so the whole set reviewed a page he never sees. Contrast
#                  between the two speakers is exactly what does not survive the swap, so the
#                  theme that counts is the one captured by default.
#   --config FILE  Use exactly this configuration file and no other. Missing file is an error.
#   --tunnel       Force the cloudflared tunnel check ON for this run.
#   --no-tunnel    Force the cloudflared tunnel check OFF for this run.
#   --tag TAG      Image tag to build and run (default: v0, or $VIBE_TALK_IMAGE_TAG).
#   --port PORT    Host port to publish on (default: 8080, or $VIBE_TALK_HOST_PORT).
#   --restart-policy POLICY
#                  Restart policy for the launched container (default: on-failure:5, or
#                  $VIBE_TALK_RESTART_POLICY). See the discussion below before changing it.
#   --build-network MODE
#                  Network mode for the BUILD only, passed straight through to the engine
#                  ('host', 'none', a named network — whatever your engine accepts). Default:
#                  unset, meaning the engine's own default, or $VIBE_TALK_BUILD_NETWORK.
#                  Reach for this when the build fails downloading dependencies while the same
#                  host can fetch them fine from a shell: the build runs in its own network
#                  namespace, and on some hosts outbound access exists only in the host's.
#                  Affects nothing about the running container, which is published on a port.
#                    ./scripts/run.sh --build-network host
#   --dry-run      Run every check and print the build/run commands, but build and start nothing.
#   --help         This text.
#
# How the container is run, and why:
#   Detached (-d), under a FIXED name (default: the image name, so 'vibe-talk'), and WITHOUT
#   --rm. Each of those is deliberate:
#     * -d          it does not die with the terminal that launched it. Follow it with --follow.
#     * fixed name  'podman logs vibe-talk' works for a human without looking anything up.
#     * no --rm     the container survives stopping, so 'podman logs' still has the per-request
#                   access log AFTER a crash — which is exactly when you want to read it. With
#                   --rm the log was destroyed by the very event you were investigating.
#     * k8s-file    the log driver is PINNED rather than inherited. Podman's default is often
#                   journald, and under journald this script's retention promise is not one it
#                   can keep: on a rootless host with no readable journal 'podman logs' returns
#                   NOTHING AT ALL — no error, just silence, which reads as "the server said
#                   nothing" rather than "you cannot see what it said". Where the journal does
#                   work the lifetime is still wrong, because entries outlive the container and
#                   rotate on the journal's schedule instead of the container's. k8s-file writes
#                   a file the container owns, which is exactly the lifetime promised below.
#   The retention guarantee, stated exactly: logs survive stopping, and are discarded at the
#   NEXT launch, when the old container is removed to make room. So look before you relaunch.
#   A leftover stopped container is removed automatically just before the relaunch, so a stale
#   one never collides with the fixed name and you never have to clean up by hand.
#
#   Detection of "is it already running?" still matches on IMAGE REFERENCE and PUBLISHED HOST
#   PORT, never on the container name. The fixed name is a convenience for humans; depending on
#   it would break the moment someone launched a container by hand, and would regress the
#   equal-length matching bug fixed in running_containers() below.
#
# Restart policy: pick it deliberately. RECOMMENDED: on-failure:5 (the default).
#   on-failure:5   Restart after a crash, at most 5 times, then stay down. A one-off crash
#                  self-heals; a crash LOOP stops and stays visibly down instead of hiding
#                  behind an endless restart, which is how an outage becomes invisible. The
#                  logs of the last attempt are still there to read.
#   always         Restart forever, including after a clean exit. Choose this only if you would
#                  rather be up-with-broken-requests than down, and you are watching --logs.
#   no             Never restart. A crash stays exactly where it fell. Most visible, least
#                  available; a fine choice while debugging.
#   unless-stopped On PODMAN this is IDENTICAL to 'always' (podman-run(1) says so) — it is NOT
#                  the docker behaviour some expect.
#   Surviving a REBOOT is a separate switch, and no policy value alone provides it: podman only
#   restarts containers across a reboot via podman-restart.service, which starts containers
#   whose policy is literally 'always'. To get reboot survival:
#     systemctl --user enable --now podman-restart.service   # and: loginctl enable-linger $USER
#   and run with --restart-policy always. Until you do that, the tunnel unit comes back after a
#   reboot and vibe-talk does not — which --tunnel-status and --status will show you.
#
# Configuration precedence, HIGHEST first:
#   1. Variables already exported in your shell environment. These always win.
#   2. --config FILE, if given. When passed, no other file is read.
#   3. ~/.config/vibe-talk/env         — per-user base, shared by every checkout on this machine.
# There is exactly ONE default configuration file, so there is never a question of which copy is
# live. Copy vibe-talk/vibe-talk.env.example to it to get started; it documents every variable.
#
# This script never prints the value of a credential. It reports variables by NAME only.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
VIBE_TALK_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"

USER_CONFIG_DEFAULT="${XDG_CONFIG_HOME:-$HOME/.config}/vibe-talk/env"
USER_CONFIG_DIR="$(dirname -- "$USER_CONFIG_DEFAULT")"
EXAMPLE_CONFIG="$VIBE_TALK_DIR/vibe-talk.env.example"

# Required for the server to work at all. A container that starts without one of these is the
# failure mode this script exists to prevent.
REQUIRED_VARS=(
    VIBE_TALK_DISCORD_BOT_TOKEN
    VIBE_TALK_READ_TOKEN
    VIBE_TALK_WRITE_TOKEN
    VIBE_TALK_CHANNELS
)
# Passed through to the container when set, ignored when not.
OPTIONAL_VARS=(
    VIBE_TALK_ELEVENLABS_API_KEY
    VIBE_TALK_ELEVENLABS_AGENT_ID
    VIBE_TALK_ELEVENLABS_API_BASE
    VIBE_TALK_PUBLIC_BASE_URL
    VIBE_TALK_TIMEZONE
    VIBE_TALK_SKIP_STARTUP_PROBE
    VIBE_TALK_STORAGE_PATH
    RUST_LOG
)
# Settings for this launcher itself (not for the server). They may be set in the same config file.
LAUNCHER_VARS=(
    VIBE_TALK_IMAGE_NAME
    VIBE_TALK_IMAGE_TAG
    VIBE_TALK_HOST_PORT
    VIBE_TALK_HOST_ADDR
    VIBE_TALK_CONTAINER_NAME
    VIBE_TALK_RESTART_POLICY
    VIBE_TALK_LOG_DRIVER
    VIBE_TALK_BUILD_NETWORK
    VIBE_TALK_TUNNEL_ENABLED
    VIBE_TALK_TUNNEL_UNIT
    VIBE_TALK_TUNNEL_CONFIG
    VIBE_TALK_ENGINE
    VIBE_TALK_DATA_DIR
)

CONFIG_FILE=""
SHUTDOWN=0
RESTART=0
DRY_RUN=0
STATUS_ONLY=0
LOGS_ONLY=0
FOLLOW=0
TUNNEL_STATUS_ONLY=0
SMOKE_AGENT=0
SMOKE_NONCE=0
SMOKE_REPLAY=0
SCREENSHOTS=0
SHOTS_OUT=""
SHOTS_THEME=""
TUNNEL_OVERRIDE=""
TAG_OVERRIDE=""
PORT_OVERRIDE=""
POLICY_OVERRIDE=""
BUILD_NETWORK_OVERRIDE=""

# The header comment above IS the help text: printed from line 2 up to the first non-comment
# line, so it cannot drift out of sync with the flags it documents.
usage() { awk 'NR>1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; }

die() { echo "" >&2; echo "vibe-talk run.sh: $*" >&2; exit 1; }
note() { echo "  $*"; }
step() { echo "==> $*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --shutdown) SHUTDOWN=1; shift ;;
        --restart) RESTART=1; shift ;;
        --config) CONFIG_FILE="${2:?--config needs a file path}"; shift 2 ;;
        --tunnel) TUNNEL_OVERRIDE=1; shift ;;
        --no-tunnel) TUNNEL_OVERRIDE=0; shift ;;
        --tag) TAG_OVERRIDE="${2:?--tag needs a value}"; shift 2 ;;
        --port) PORT_OVERRIDE="${2:?--port needs a value}"; shift 2 ;;
        --restart-policy) POLICY_OVERRIDE="${2:?--restart-policy needs a value}"; shift 2 ;;
        --build-network) BUILD_NETWORK_OVERRIDE="${2:?--build-network needs a value}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --status) STATUS_ONLY=1; shift ;;
        --logs) LOGS_ONLY=1; shift ;;
        --follow) FOLLOW=1; shift ;;
        --tunnel-status) TUNNEL_STATUS_ONLY=1; shift ;;
        --smoke-agent) SMOKE_AGENT=1; shift ;;
        --nonce) SMOKE_NONCE=1; shift ;;
        --replay-check) SMOKE_REPLAY=1; shift ;;
        --screenshots) SCREENSHOTS=1; shift ;;
        --out) SHOTS_OUT="${2:?--out needs a directory}"; shift 2 ;;
        --theme) SHOTS_THEME="${2:?--theme needs dark, light or both}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unrecognized argument: $1" >&2; echo "" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$SHUTDOWN" = 1 ] && [ "$RESTART" = 1 ]; then
    die "--shutdown and --restart contradict each other. --shutdown stops and exits; --restart stops and then relaunches."
fi

# Every one of these ends the run in a different place, so asking for two of them at once is
# always a mistake, and a silently-ignored flag is the worst way to answer it.
# (Written as `if` blocks, not `[ ... ] && ...`: under `set -e` a trailing false test on the last
# such line would exit the script.)
ACTIONS_GIVEN=()
if [ "$SHUTDOWN" = 1 ]; then ACTIONS_GIVEN+=(--shutdown); fi
if [ "$RESTART" = 1 ]; then ACTIONS_GIVEN+=(--restart); fi
if [ "$STATUS_ONLY" = 1 ]; then ACTIONS_GIVEN+=(--status); fi
if [ "$LOGS_ONLY" = 1 ]; then ACTIONS_GIVEN+=(--logs); fi
if [ "$FOLLOW" = 1 ]; then ACTIONS_GIVEN+=(--follow); fi
if [ "$TUNNEL_STATUS_ONLY" = 1 ]; then ACTIONS_GIVEN+=(--tunnel-status); fi
if [ "$SMOKE_AGENT" = 1 ]; then ACTIONS_GIVEN+=(--smoke-agent); fi
if [ "$SCREENSHOTS" = 1 ]; then ACTIONS_GIVEN+=(--screenshots); fi
if [ "${#ACTIONS_GIVEN[@]}" -gt 1 ]; then
    die "these actions cannot be combined: ${ACTIONS_GIVEN[*]}
Each one ends the run somewhere different. Pick exactly one and re-run.
  --logs dumps output and exits;  --follow streams it;  --status and --tunnel-status report;
  --shutdown stops and exits;     --restart stops and relaunches."
fi

if [ "$SMOKE_REPLAY" = 1 ] && [ "$SMOKE_AGENT" != 1 ]; then
    die "--replay-check only means something together with --smoke-agent.
It is a different question asked through the same harness: whether the VENDOR acts on a replayed
transcript. On its own there is no conversation for it to hold, and accepting it silently would
leave you believing you had checked resuming when you had not."
fi

if [ "$SMOKE_NONCE" = 1 ] && [ "$SMOKE_REPLAY" = 1 ]; then
    die "--nonce and --replay-check ask different questions of different rounds.
--nonce strengthens the relay check by posting a token to the CHANNEL; --replay-check does not
run the relay check at all. Pick one and re-run."
fi

if [ "$SMOKE_NONCE" = 1 ] && [ "$SMOKE_AGENT" != 1 ]; then
    die "--nonce only means something together with --smoke-agent.
It is the strong form of that check: it posts one unique token to the channel through this
server's own write API and requires the agent to relay it back verbatim. On its own there is
nothing for it to modify, and accepting it silently would leave you believing you had run the
strong check when you had not."
fi

if [ -n "$SHOTS_THEME" ] && [ "$SCREENSHOTS" != 1 ]; then
    die "--theme only means something together with --screenshots.
It picks the colour scheme the /voice page is photographed in. On its own there is nothing to
photograph, and accepting it silently would leave you believing you had chosen a theme for a run
that never took a picture."
fi

if [ -n "$SHOTS_OUT" ] && [ "$SCREENSHOTS" != 1 ]; then
    die "--out only means something together with --screenshots.
It names the directory the /voice screenshots are written to. On its own there is nothing to
write, and accepting it silently would leave you believing you had chosen an output directory for
a run that never took a picture."
fi

# ---------------------------------------------------------------------------------------------
# Screenshots of the /voice page. FREE, offline, and deliberately self-contained.
#
# It is handled HERE, before any configuration is loaded, because it needs NONE of the owner's
# configuration and must never read it: it stands up its own throwaway server with its own
# throwaway credentials against an in-memory Discord. Reading ~/.config/vibe-talk/env would give
# this action the real bot token for no reason, and would make it fail on a machine that has no
# deployment at all — which is exactly the machine where you most want to look at the page.
#
# Nothing here touches the owner's container. It publishes no port the deployment uses, builds no
# image, stops nothing and starts no container.
#
# It now starts TWO processes, not one: the throwaway server, and vibe-talk-mock-elevenlabs beside
# it. That is not scope creep. Message summaries come from a conversation with an ElevenLabs agent
# and from nothing else, so a run with no vendor behind it cannot photograph a summarised channel
# at all -- it waits twenty seconds and reports the state as unreachable. The mock is this
# repository's own loopback vendor: free, offline, bound to loopback, and holding a real
# conversation over a real WebSocket, which also makes those two frames evidence about the real
# socket client rather than about a page-local stub. Both processes are killed by the same trap.
# ---------------------------------------------------------------------------------------------

if [ "$SCREENSHOTS" = 1 ]; then
    SHOTS_SCRIPT="$SCRIPT_DIR/screenshots.py"
    [ -x "$SHOTS_SCRIPT" ] || die "the screenshot harness is missing or not executable: $SHOTS_SCRIPT"

    SHOTS_PORT="${PORT_OVERRIDE:-${VIBE_TALK_SHOTS_PORT:-18091}}"
    case "$SHOTS_PORT" in
        ''|*[!0-9]*) die "--port must be a number, got: $SHOTS_PORT" ;;
    esac
    # Named, not merely "in use". 8080 serves the owner's live agent and 18081 is the unrelated
    # vibe-talk:ci container; a screenshot run that bound either would take down something real to
    # photograph something fake. The refusal says which one and why, because "port in use" would
    # send the reader looking for a stale process that is not the problem.
    if [ "$SHOTS_PORT" = 8080 ]; then
        die "--screenshots refuses port 8080. That is the LIVE vibe-talk, serving the owner's
agent. This action stands up a throwaway server of its own and must never contend with it.
Leave --port off to use $((18091)), or pass a different one."
    fi
    if [ "$SHOTS_PORT" = 18081 ]; then
        die "--screenshots refuses port 18081. That is the unrelated vibe-talk:ci container.
Leave --port off to use 18091, or pass a different one."
    fi
    # Anything else already listening is a stale run or a coincidence, and binding on top of it
    # would photograph somebody else's server.
    if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | grep -Eq "[:.]${SHOTS_PORT}[[:space:]]"; then
        die "something is already listening on port $SHOTS_PORT.
Screenshots would then be taken against whatever that is, not against the server this action
starts. Stop it, or choose another port with --port."
    fi

    # THE OFFLINE VENDOR, and it is no longer optional. Message summaries come from the ElevenLabs
    # agent and from nothing else -- the extractive summariser that used to answer for free has
    # been deleted -- so a throwaway server with no [elevenlabs] section cannot produce a single
    # summary, and the state that photographs one would hang for twenty seconds and then report an
    # unreachable page. vibe-talk-mock-elevenlabs is this repository's own loopback vendor: a real
    # mint endpoint and a real conversation WebSocket, free, offline, and answering canned prose.
    #
    # Two ports, derived from the one the caller chose rather than fixed, so `--port` still moves
    # the whole run out of the way of anything else instead of half of it.
    SHOTS_MOCK_HTTP_PORT=$((SHOTS_PORT + 1))
    SHOTS_MOCK_WS_PORT=$((SHOTS_PORT + 2))
    for shots_extra_port in "$SHOTS_MOCK_HTTP_PORT" "$SHOTS_MOCK_WS_PORT"; do
        if [ "$shots_extra_port" = 8080 ] || [ "$shots_extra_port" = 18081 ]; then
            die "--port $SHOTS_PORT puts the offline ElevenLabs mock on port $shots_extra_port,
which is either the LIVE vibe-talk (8080) or the unrelated vibe-talk:ci container (18081). The
mock takes the TWO PORTS ABOVE the one you chose, so choose one whose two successors are neither.
Leave --port off to use 18091, 18092 and 18093, which are reserved for exactly this."
        fi
        if command -v ss >/dev/null 2>&1 &&
            ss -ltn 2>/dev/null | grep -Eq "[:.]${shots_extra_port}[[:space:]]"; then
            die "something is already listening on port $shots_extra_port, which is where the
offline ElevenLabs mock has to bind (it takes the two ports above --port $SHOTS_PORT). Stop it, or
choose another port with --port."
        fi
    done

    command -v cargo >/dev/null 2>&1 || die "--screenshots needs cargo, and it is not on PATH.
The /voice page is compiled INTO the server binary (web/voice.html and web/voice.js are
include_str! constants), so a stale binary serves last build's markup. Install Rust, or build
the binary yourself and re-run."

    SHOTS_STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
    if [ -z "$SHOTS_OUT" ]; then
        SHOTS_OUT="$VIBE_TALK_DIR/debug/screenshots/$SHOTS_STAMP"
    fi

    # Throwaway credentials. They are 24+ characters because the server refuses shorter ones, and
    # they are visibly not real so that finding one in a log is never confusing. Nothing they
    # authenticate against exists outside this run.
    SHOTS_READ_TOKEN="screenshot-run-read-token-not-a-real-credential"
    SHOTS_WRITE_TOKEN="screenshot-run-write-token-not-a-real-credential"
    SHOTS_CHANNEL="100000000000000001"

    SHOTS_TMP="$(mktemp -d)"
    SHOTS_PID=""
    SHOTS_MOCK_PID=""
    shots_cleanup() {
        # BOTH processes, and the mock as carefully as the server. It holds two ports of its own,
        # so an orphaned one makes the NEXT run refuse to start for a reason that names a port and
        # not the run that leaked it -- which is exactly how the six-day orphan below was found.
        for shots_leftover in "$SHOTS_PID" "$SHOTS_MOCK_PID"; do
            if [ -n "$shots_leftover" ] && kill -0 "$shots_leftover" 2>/dev/null; then
                kill "$shots_leftover" 2>/dev/null || true
                wait "$shots_leftover" 2>/dev/null || true
            fi
        done
        rm -rf "$SHOTS_TMP"
    }
    # On failure too: a throwaway server left running would hold the port and the next run would
    # refuse to start, for a reason that looks nothing like the failure that caused it.
    #
    # INT, TERM and HUP as well as EXIT, and that is not belt-and-braces. An EXIT trap alone does
    # not run when the shell is killed by a signal, so a Ctrl-C or a closed terminal orphaned the
    # server: it was reparented to user systemd and went on holding its port. One was found still
    # running SIX DAYS after the run that started it, with its own config directory long deleted.
    # `kill 0` is deliberately not used -- this kills the server it started, not the caller's
    # process group.
    trap shots_cleanup EXIT INT TERM HUP

    # Static prose is in QUOTED heredocs. Backticks in these TOML comments are deliberate
    # regression inputs: an unquoted heredoc treats them as command substitutions, which once ran
    # the screenshot state names as shell commands before the server even started. Dynamic values
    # are emitted separately, one controlled value per printf.
    {
        printf '[server]\nbind = "127.0.0.1:%s"\n' "$SHOTS_PORT"
        cat <<'EOF'

[discord]
bot_token = "screenshot-run-bot-token-never-sent-anywhere"
# Deliberately SMALL, and this is the only reason it is here: --fake-discord seeds about a dozen
# messages, so at the default ceiling the channel arrives in one read and the walk `#65
# scrollback-paging` added is never exercised at all. With a small ceiling the seeded channel
# really pages, the "Older messages" control really appears, and 21-channel-older-loaded is a
# picture of a server-cursored step rather than of a control that happens to be rendered.
#
# SIX, not eight, and the difference is a whole page. Fourteen seeded messages at eight arrive in
# exactly two reads, so one step back reaches the beginning -- and `31-pull-to-refresh-armed` is
# specifically the channel that STILL has history above the reader after that step, which is the
# case pull-to-refresh exists for. At eight there is no such position to photograph. At six the
# channel pages three times, which also keeps `10-discord-view` above the five rows it asserts.
default_fetch_limit = 6
max_fetch_limit = 6
EOF

        printf '\n[auth]\nread_token = "%s"\nwrite_token = "%s"\n' \
            "$SHOTS_READ_TOKEN" "$SHOTS_WRITE_TOKEN"
        printf '\n[[channels]]\nid = "%s"\nlabel = "lead team"\nwritable = true\n' \
            "$SHOTS_CHANNEL"
        cat <<'EOF'

# Pointed at the offline mock started above, NOT at ElevenLabs. The agent id and key are the mock's
# own published constants; they authenticate against an account nobody owns and mean nothing
# anywhere else. This is what makes the summary states reachable at all: there is one summariser
# now, it is a conversation with an agent, and without a vendor to hold it every long message comes
# back as a failure.
[elevenlabs]
agent_id = "agent_mock00000000000000000000000"
api_key = "xi-mock-api-key-not-a-real-one"
EOF
        printf 'api_base = "http://127.0.0.1:%s/v1"\n' "$SHOTS_MOCK_HTTP_PORT"
        cat <<'EOF'

# Three seconds, not the default thirty, and it is the whole reason the FAILURE state is
# photographable in under a minute: the state that pictures a red row wedges the mock so that it
# answers nothing at all, and the deadline is what turns that into a failure the page can draw.
# It is folded into the summary cache key like everything else here, which costs nothing on a
# store that is deleted with the run.
[summaries]
reply_timeout_seconds = 3

# Storage inside the throwaway directory, which is removed with it. The harness drives real calls
# through a real server, so without this line either the run would have no durable state to
# photograph, or -- worse, if it inherited a real path -- it would write the screenshot script's
# invented conversations into the owner's own store.
[storage]
EOF
        printf 'path = "%s/state/vibe-talk.sqlite3"\n' "$SHOTS_TMP"
    } > "$SHOTS_TMP/vibe-talk.toml"

    case "${SHOTS_THEME:-dark}" in
        dark|light|both) ;;
        *) die "--theme must be dark, light or both; got: $SHOTS_THEME" ;;
    esac

    step "Screenshots of /voice — free, offline, no vendor conversation and no microphone."
    note "theme:     ${SHOTS_THEME:-dark} (dark by default — it is the theme the phone is in)"
    note "server:    throwaway, native, --fake-discord, 127.0.0.1:$SHOTS_PORT (never 8080)"
    note "vendor:    vibe-talk-mock-elevenlabs on 127.0.0.1:$SHOTS_MOCK_HTTP_PORT (mint + control)"
    note "           and 127.0.0.1:$SHOTS_MOCK_WS_PORT (conversations). Offline; nothing is billed."
    note "output:    $SHOTS_OUT"
    note "the running vibe-talk, its config and its logs are not touched."

    if [ "$DRY_RUN" = 1 ]; then
        note "(dry run) cargo build --release --manifest-path $VIBE_TALK_DIR/Cargo.toml"
        note "(dry run) $VIBE_TALK_DIR/target/release/vibe-talk-mock-elevenlabs --scenario no_tool_call --http-port $SHOTS_MOCK_HTTP_PORT --ws-port $SHOTS_MOCK_WS_PORT --bridge-base http://127.0.0.1:$SHOTS_PORT --bridge-token <write token>"
        note "(dry run) $VIBE_TALK_DIR/target/release/vibe-talk --config $SHOTS_TMP/vibe-talk.toml --fake-discord"
        note "(dry run) $SHOTS_SCRIPT --url http://127.0.0.1:$SHOTS_PORT --channel $SHOTS_CHANNEL --out $SHOTS_OUT --theme ${SHOTS_THEME:-dark} --mock-control http://127.0.0.1:$SHOTS_MOCK_HTTP_PORT"
        note "(dry run) nothing was built, started, or photographed."
        exit 0
    fi

    step "Rebuilding the server, because web/ is compiled into it"
    cargo build --release --manifest-path "$VIBE_TALK_DIR/Cargo.toml" \
        || die "the build failed, so there is no binary serving today's web/ to photograph."
    SHOTS_BIN="$VIBE_TALK_DIR/target/release/vibe-talk"
    [ -x "$SHOTS_BIN" ] || die "the build reported success but $SHOTS_BIN is not there."
    SHOTS_MOCK_BIN="$VIBE_TALK_DIR/target/release/vibe-talk-mock-elevenlabs"
    [ -x "$SHOTS_MOCK_BIN" ] || die "the build reported success but $SHOTS_MOCK_BIN is not there.
That binary is the offline ElevenLabs substitute, and the summary states cannot be photographed
without it: summaries come from a conversation with an agent and from nothing else."

    step "Starting the offline ElevenLabs mock"
    # BEFORE the server, so the server's own configuration points at something that is already
    # listening. It costs nothing either way -- vibe-talk opens no vendor connection at startup --
    # but a mock that failed to bind is then a failure with its own message rather than a summary
    # state timing out four minutes later.
    #
    # `no_tool_call` is the scenario that answers canned prose without calling anything, so this
    # needs no MCP traffic back to the bridge. --bridge-base and --bridge-token are still required
    # by the binary, and are given the throwaway server and its throwaway token: the token is on a
    # command line, which everything else here refuses to do, and that is acceptable only because
    # this particular credential is invented per run and authenticates nothing that exists.
    (
        cd "$SHOTS_TMP" || exit 1
        exec "$SHOTS_MOCK_BIN" \
            --scenario no_tool_call \
            --http-port "$SHOTS_MOCK_HTTP_PORT" \
            --ws-port "$SHOTS_MOCK_WS_PORT" \
            --bridge-base "http://127.0.0.1:$SHOTS_PORT" \
            --bridge-token "$SHOTS_WRITE_TOKEN" \
            > "$SHOTS_TMP/mock-elevenlabs.log" 2>&1
    ) &
    SHOTS_MOCK_PID=$!

    SHOTS_MOCK_UP=0
    for _ in $(seq 1 60); do
        if ! kill -0 "$SHOTS_MOCK_PID" 2>/dev/null; then break; fi
        if curl -fsS -o /dev/null "http://127.0.0.1:$SHOTS_MOCK_HTTP_PORT/_mock/trace" 2>/dev/null; then
            SHOTS_MOCK_UP=1
            break
        fi
        sleep 0.5
    done
    if [ "$SHOTS_MOCK_UP" != 1 ]; then
        echo "" >&2
        echo "--- offline ElevenLabs mock output ---" >&2
        cat "$SHOTS_TMP/mock-elevenlabs.log" >&2 || true
        die "the offline ElevenLabs mock never answered on 127.0.0.1:$SHOTS_MOCK_HTTP_PORT.
Its output is above. Without it every summary state is unreachable."
    fi
    note "mock up on 127.0.0.1:$SHOTS_MOCK_HTTP_PORT (pid $SHOTS_MOCK_PID)"

    step "Starting the throwaway server"
    (
        cd "$SHOTS_TMP" || exit 1
        exec "$SHOTS_BIN" --config "$SHOTS_TMP/vibe-talk.toml" --fake-discord \
            > "$SHOTS_TMP/server.log" 2>&1
    ) &
    SHOTS_PID=$!

    SHOTS_UP=0
    for _ in $(seq 1 60); do
        if ! kill -0 "$SHOTS_PID" 2>/dev/null; then break; fi
        if curl -fsS -o /dev/null "http://127.0.0.1:$SHOTS_PORT/healthz" 2>/dev/null; then
            SHOTS_UP=1
            break
        fi
        sleep 0.5
    done
    if [ "$SHOTS_UP" != 1 ]; then
        echo "" >&2
        echo "--- throwaway server output ---" >&2
        cat "$SHOTS_TMP/server.log" >&2 || true
        die "the throwaway server never answered on 127.0.0.1:$SHOTS_PORT. Its output is above."
    fi
    note "up on 127.0.0.1:$SHOTS_PORT (pid $SHOTS_PID)"

    # Through the environment, never the command line, for the same reason as the smoke test: a
    # command line is readable by every process on this box.
    export VIBE_TALK_WRITE_TOKEN="$SHOTS_WRITE_TOKEN"
    rc=0
    "$SHOTS_SCRIPT" \
        --url "http://127.0.0.1:$SHOTS_PORT" \
        --channel "$SHOTS_CHANNEL" \
        --out "$SHOTS_OUT" \
        --theme "${SHOTS_THEME:-dark}" \
        --mock-control "http://127.0.0.1:$SHOTS_MOCK_HTTP_PORT" || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "" >&2
        echo "The screenshot run FAILED (exit $rc). Nothing above is evidence about how the page" >&2
        echo "looks; see the exit-code table in $SHOTS_SCRIPT for what this one means." >&2
        exit "$rc"
    fi
    exit 0
fi

# ---------------------------------------------------------------------------------------------
# 2 (first half). Load configuration.
#
# Precedence is implemented by remembering which variables the CALLING environment already set,
# sourcing the config file, then restoring the caller's values on top. That is what makes an
# exported variable beat the file without the file having to know about it.
# ---------------------------------------------------------------------------------------------

ALL_VARS=("${REQUIRED_VARS[@]}" "${OPTIONAL_VARS[@]}" "${LAUNCHER_VARS[@]}")

declare -A PRESET_VALUES=()
for var in "${ALL_VARS[@]}"; do
    if [ -n "${!var+x}" ]; then PRESET_VALUES["$var"]="${!var}"; fi
done

SOURCED_FILES=()
source_config() {
    local file="$1"
    # `set -u` is relaxed only across the source: config files legitimately reference variables
    # that may not be set yet (e.g. VAR="${VAR:-default}" style lines).
    set +u
    # shellcheck disable=SC1090
    . "$file"
    set -u
    SOURCED_FILES+=("$file")
}

if [ -n "$CONFIG_FILE" ]; then
    [ -f "$CONFIG_FILE" ] || die "--config file not found: $CONFIG_FILE"
    [ -r "$CONFIG_FILE" ] || die "--config file is not readable: $CONFIG_FILE"
    source_config "$CONFIG_FILE"
else
    if [ -f "$USER_CONFIG_DEFAULT" ]; then source_config "$USER_CONFIG_DEFAULT"; fi
fi

for var in "${!PRESET_VALUES[@]}"; do
    printf -v "$var" '%s' "${PRESET_VALUES[$var]}"
done

if [ "${#SOURCED_FILES[@]}" -eq 0 ]; then
    die "no configuration file found.

Looked for:
  $USER_CONFIG_DEFAULT

Create it with:
  mkdir -p $USER_CONFIG_DIR
  cp $EXAMPLE_CONFIG $USER_CONFIG_DEFAULT
  \$EDITOR $USER_CONFIG_DEFAULT

That example documents every variable and marks which are required. That file holds credentials:
it lives outside any git checkout on purpose, so keep it there and readable only by you."
fi

# ---------------------------------------------------------------------------------------------
# Launcher settings, after config load so the config file can set them.
# ---------------------------------------------------------------------------------------------

IMAGE_NAME="${VIBE_TALK_IMAGE_NAME:-vibe-talk}"
IMAGE_TAG="${TAG_OVERRIDE:-${VIBE_TALK_IMAGE_TAG:-v0}}"
HOST_PORT="${PORT_OVERRIDE:-${VIBE_TALK_HOST_PORT:-8080}}"
HOST_ADDR="${VIBE_TALK_HOST_ADDR:-127.0.0.1}"
TUNNEL_UNIT="${VIBE_TALK_TUNNEL_UNIT:-cloudflared-vibe-talk.service}"
TUNNEL_CONFIG="${VIBE_TALK_TUNNEL_CONFIG:-$HOME/.cloudflared/config.yml}"
ENGINE="${VIBE_TALK_ENGINE:-podman}"
# The HOST directory bind-mounted at /var/lib/vibe-talk. This mount, not the image's VOLUME
# declaration, is what makes durable state actually durable: without it the store lives in the
# container's writable layer and is destroyed the next time this script replaces the container —
# which it does on every launch. An anonymous podman volume would survive but is trivially
# orphaned and impossible to find later, so the explicit path is the mechanism.
DATA_DIR="${VIBE_TALK_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/vibe-talk}"
CONTAINER_DATA_DIR=/var/lib/vibe-talk
IMAGE_REF="${IMAGE_NAME}:${IMAGE_TAG}"
# Defaults to the image name, so the self-test's throwaway image gets a throwaway container name
# and can never collide with the real 'vibe-talk'.
CONTAINER_NAME="${VIBE_TALK_CONTAINER_NAME:-$IMAGE_NAME}"
RESTART_POLICY="${POLICY_OVERRIDE:-${VIBE_TALK_RESTART_POLICY:-on-failure:5}}"
# Pinned, not inherited — see "How the container is run, and why" above for what inheriting it
# costs. The two engines spell the same thing differently: podman writes a container-owned file
# under 'k8s-file', docker under 'json-file'. Overridable for an operator who has a log pipeline
# of their own and wants the container to feed it instead.
if [ -n "${VIBE_TALK_LOG_DRIVER:-}" ]; then
    LOG_DRIVER="$VIBE_TALK_LOG_DRIVER"
elif [ "$(basename "$ENGINE")" = docker ]; then
    LOG_DRIVER=json-file
else
    LOG_DRIVER=k8s-file
fi
# Empty by default, which means "say nothing and let the engine choose" — the behaviour this
# script has always had. It exists because the build gets its own network namespace, and on a host
# where outbound access lives only in the host's, the build fails fetching dependencies while the
# very same host fetches them fine from a shell. That failure is indistinguishable from a broken
# Containerfile until you know the build is not on the network you think it is, and before this
# there was no lever here to try: BUILD only, never the running container.
BUILD_NETWORK="${BUILD_NETWORK_OVERRIDE:-${VIBE_TALK_BUILD_NETWORK:-}}"

case "$HOST_PORT" in
    ''|*[!0-9]*) die "host port must be a number, got: $HOST_PORT" ;;
esac

# Container names are [a-zA-Z0-9][a-zA-Z0-9_.-]*; anything else is rejected by the engine with a
# message that does not mention this script, minutes into a launch.
if ! [[ "$CONTAINER_NAME" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; then
    die "container name is not usable: '$CONTAINER_NAME'
It must start with a letter or digit and contain only letters, digits, '_', '.' or '-'.
Set VIBE_TALK_CONTAINER_NAME (or VIBE_TALK_IMAGE_NAME, which it defaults to)."
fi

# Checked here rather than at `run` time: an unknown policy is otherwise discovered AFTER the
# image has been rebuilt and the old container stopped.
if ! [[ "$RESTART_POLICY" =~ ^(no|never|always|unless-stopped|on-failure(:[0-9]+)?)$ ]]; then
    die "restart policy is not one podman accepts: '$RESTART_POLICY'
Valid: no, never, on-failure, on-failure:<max-retries>, always, unless-stopped.
Recommended: on-failure:5 — see --help for why, and for what it takes to survive a reboot."
fi

TUNNEL_ENABLED_RAW="${TUNNEL_OVERRIDE:-${VIBE_TALK_TUNNEL_ENABLED:-0}}"
case "$(printf '%s' "$TUNNEL_ENABLED_RAW" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes|on) TUNNEL_ENABLED=1 ;;
    0|false|no|off|'') TUNNEL_ENABLED=0 ;;
    *) die "VIBE_TALK_TUNNEL_ENABLED must be one of 1/0/true/false/yes/no/on/off, got: $TUNNEL_ENABLED_RAW" ;;
esac

command -v "$ENGINE" >/dev/null 2>&1 \
    || die "container engine '$ENGINE' not found on PATH. Install podman, or set VIBE_TALK_ENGINE=docker."

# ---------------------------------------------------------------------------------------------
# 1. Already-running check.
#
# Matched by IMAGE reference or by PUBLISHED HOST PORT — never by container name. `podman run`
# without --name invents a new name every launch, so a name match is guaranteed to go stale, and
# matching a name prefix would also sweep up unrelated containers.
# ---------------------------------------------------------------------------------------------

matching_containers() {  # matching_containers [extra `ps` flags, e.g. -a]
    "$ENGINE" ps "$@" --format '{{.ID}}|{{.Image}}|{{.Names}}|{{.Ports}}|{{.Status}}' 2>/dev/null \
        | awk -F'|' -v img="$IMAGE_REF" -v port="$HOST_PORT" '
            {
                # A registry-qualified image reads "localhost/vibe-talk:v0", so accept an exact
                # match or a "/"-prefixed suffix match. The index() > 0 guard is load-bearing:
                # index() returns 0 when the substring is ABSENT, and 0 also equals
                # length($2) - length(img) whenever the two references happen to be the same
                # length -- so without it, "vibe-talk:dryrun-probe" matched the unrelated
                # "localhost/vibe-talk:ci". A false match here is not cosmetic: under --shutdown
                # or --restart this function decides what gets STOPPED.
                slash_at = index($2, "/" img)
                image_match = ($2 == img) || (slash_at > 0 && slash_at == length($2) - length(img))
                # A published port renders as e.g. "127.0.0.1:8080->8080/tcp"; requiring the
                # colon immediately before the number keeps ":8080->" from matching ":18080->".
                port_match = (index($4, ":" port "->") > 0)
                if (image_match || port_match) print $0
            }'
}

running_containers() { matching_containers; }
# `ps -a` also lists stopped ones. Podman still reports their published ports there, so the same
# image-or-port rule applies unchanged.
all_containers() { matching_containers -a; }

# The stopped ones are "all, minus the running ones", compared by container ID. Deriving it that
# way rather than by pattern-matching the Status column means no wording of "Exited (137) ..." or
# "Created" can be misread as running.
stopped_containers() {
    local running_ids line id
    running_ids="$(running_containers | cut -d'|' -f1)"
    all_containers | while IFS= read -r line; do
        id="${line%%|*}"
        [ -n "$id" ] || continue
        if ! grep -qxF -- "$id" <<< "$running_ids"; then printf '%s\n' "$line"; fi
    done
}

# Reports "<policy>|<autoremove>" for one container, e.g. "on-failure:5|false".
container_run_mode() {
    local id="$1" raw name count autoremove
    raw="$("$ENGINE" inspect --format \
        '{{.HostConfig.RestartPolicy.Name}}|{{.HostConfig.RestartPolicy.MaximumRetryCount}}|{{.HostConfig.AutoRemove}}' \
        "$id" 2>/dev/null)" || raw=""
    if [ -z "$raw" ]; then echo "unknown|unknown"; return 0; fi
    IFS='|' read -r name count autoremove <<< "$raw"
    [ -n "$name" ] || name="(none)"
    if [ "$name" = "on-failure" ] && [ -n "$count" ] && [ "$count" != 0 ]; then name="on-failure:$count"; fi
    echo "${name}|${autoremove}"
}

stop_running() {
    local found="$1" id name
    while IFS='|' read -r id _image name _ports _status; do
        [ -n "$id" ] || continue
        step "Stopping container $name ($id)"
        if [ "$DRY_RUN" = 1 ]; then
            note "(dry run) $ENGINE stop $id"
        else
            "$ENGINE" stop "$id" >/dev/null \
                || die "failed to stop container $id ($name). Stop it by hand: $ENGINE stop $id"
        fi
    done <<< "$found"
}

remove_stopped() {
    local found="$1" id name
    while IFS='|' read -r id _image name _ports _status; do
        [ -n "$id" ] || continue
        step "Removing stopped container $name ($id) to free the name and the port"
        note "its logs go with it — read them first with --logs if you still need them."
        if [ "$DRY_RUN" = 1 ]; then
            note "(dry run) $ENGINE rm $id"
        else
            "$ENGINE" rm "$id" >/dev/null \
                || die "failed to remove stopped container $id ($name). Remove it by hand: $ENGINE rm $id"
        fi
    done <<< "$found"
}

# --------------------------------------------------------------------------------------------
# Tunnel status. STRICTLY READ-ONLY: it queries systemd and reads the cloudflared config, and
# starts, stops and enables nothing. Returns 0 only when the unit is active.
#
# Of the cloudflared config it prints ONLY ingress hostnames. The same file holds the tunnel id
# and the path of the credentials file, and neither belongs in this script's output.
# --------------------------------------------------------------------------------------------

tunnel_hostnames() {
    [ -r "$TUNNEL_CONFIG" ] || return 1
    grep -E '^[[:space:]]*-?[[:space:]]*hostname:[[:space:]]*' "$TUNNEL_CONFIG" 2>/dev/null \
        | sed -E 's/^[[:space:]]*-?[[:space:]]*hostname:[[:space:]]*//; s/[[:space:]]+$//; s/^"(.*)"$/\1/; s/^'"'"'(.*)'"'"'$/\1/'
}

tunnel_status_report() {
    step "Tunnel status: $TUNNEL_UNIT (systemctl --user)"
    note "read-only: this action starts, stops and enables nothing."

    if ! command -v systemctl >/dev/null 2>&1; then
        note "unit:      CANNOT TELL — systemctl is not on PATH (not a systemd host)."
        return 1
    fi

    local load active sub since enabled
    load="$(systemctl --user show "$TUNNEL_UNIT" -p LoadState --value 2>/dev/null || true)"
    if [ "$load" != "loaded" ]; then
        note "unit:      NOT INSTALLED (systemd LoadState=${load:-unknown})"
        note "expected:  ~/.config/systemd/user/$TUNNEL_UNIT"
        note "install:   write that unit (it runs 'cloudflared tunnel run <name>' and reads"
        note "           ~/.cloudflared/config.yml), then:"
        note "             systemctl --user daemon-reload"
        note "             systemctl --user enable --now $TUNNEL_UNIT"
        return 1
    fi

    active="$(systemctl --user show "$TUNNEL_UNIT" -p ActiveState --value 2>/dev/null || true)"
    sub="$(systemctl --user show "$TUNNEL_UNIT" -p SubState --value 2>/dev/null || true)"
    since="$(systemctl --user show "$TUNNEL_UNIT" -p ActiveEnterTimestamp --value 2>/dev/null || true)"
    enabled="$(systemctl --user is-enabled "$TUNNEL_UNIT" 2>/dev/null || true)"

    note "unit:      installed"
    if [ "$active" = "active" ]; then
        note "active:    YES (ActiveState=$active, SubState=$sub)"
        note "since:     ${since:-unknown}"
    else
        note "active:    NO (ActiveState=${active:-unknown}, SubState=${sub:-unknown})"
        note "start it:  systemctl --user start $TUNNEL_UNIT"
        note "diagnose:  journalctl --user -u $TUNNEL_UNIT -n 50"
    fi
    note "at boot:   ${enabled:-unknown}$([ "$enabled" = enabled ] && echo "" || echo "  (it will NOT come back after a reboot; fix with: systemctl --user enable $TUNNEL_UNIT && loginctl enable-linger \"\$USER\")")"

    local hosts
    hosts="$(tunnel_hostnames || true)"
    if [ -n "$hosts" ]; then
        note "hostname:  $(printf '%s' "$hosts" | paste -sd', ' -)"
        note "           (ingress hostnames from $TUNNEL_CONFIG)"
    else
        note "hostname:  unknown — no ingress hostname readable in $TUNNEL_CONFIG"
        note "           (set VIBE_TALK_TUNNEL_CONFIG if the tunnel config lives elsewhere)"
    fi
    note "origin:    the tunnel is configured independently of this script; it points at"
    note "           whatever host port its own ingress says, normally ${HOST_ADDR}:${HOST_PORT}."

    [ "$active" = "active" ]
}

if [ "$TUNNEL_STATUS_ONLY" = 1 ]; then
    rc=0
    tunnel_status_report || rc=$?
    exit "$rc"
fi

FOUND="$(running_containers || true)"

if [ "$STATUS_ONLY" = 1 ]; then
    STOPPED="$(stopped_containers || true)"
    step "vibe-talk status"
    note "image:          $IMAGE_REF"
    note "publish:        ${HOST_ADDR}:${HOST_PORT}"
    note "container name: $CONTAINER_NAME  (fixed, so '$ENGINE logs $CONTAINER_NAME' just works)"
    note "restart policy: $RESTART_POLICY  (applied at the next launch)"
    note "logs:           RETAINED after the container stops (it is not run with --rm)."
    note "                read them with: $SCRIPT_DIR/run.sh --logs   (or --follow to stream)"
    note "                they are discarded at the next launch, when the old container is removed."
    note "                driver: $LOG_DRIVER (pinned at launch, so the retention above is this"
    note "                script's to keep rather than the host default's to decide)."
    note "config file:    ${SOURCED_FILES[*]}"
    if [ -d "$DATA_DIR" ]; then
        note "durable state:  $DATA_DIR -> $CONTAINER_DATA_DIR  (exists; transcripts and read marks survive a rebuild)"
    else
        note "durable state:  $DATA_DIR -> $CONTAINER_DATA_DIR  (NOT created yet; the next launch creates it 0700)"
    fi
    note "tunnel check:   $([ "$TUNNEL_ENABLED" = 1 ] && echo "enabled (unit $TUNNEL_UNIT)" || echo "disabled")   — see --tunnel-status for the unit itself"
    if [ -n "$FOUND" ]; then
        note "running:"
        while IFS='|' read -r id image name ports status; do
            [ -n "$id" ] || continue
            note "  $id  $image  name=$name  ports=$ports  $status"
            mode="$(container_run_mode "$id")"
            note "      restart-policy=${mode%%|*}  autoremove=${mode##*|}"
            if [ "${mode##*|}" = "true" ]; then
                note "      NOTE: this one was started the OLD way (--rm): its logs are destroyed the"
                note "            moment it stops, so a crash takes its own evidence with it. Cut over"
                note "            with: $SCRIPT_DIR/run.sh --restart"
            fi
        done <<< "$FOUND"
    else
        note "running:        nothing matching $IMAGE_REF or host port $HOST_PORT"
    fi
    # Reboot survival is NOT a property of the restart policy on its own (see --help): podman
    # restores containers across a reboot only through podman-restart.service, and that unit
    # starts only containers whose policy is literally 'always'. Reporting the two facts
    # separately is the point — either one alone reads as "it will come back" when it will not.
    prs_state="unknown (no systemctl)"
    if command -v systemctl >/dev/null 2>&1; then
        prs_state="$(systemctl --user is-enabled podman-restart.service 2>/dev/null || echo disabled)"
    fi
    note "after a reboot: podman-restart.service is $prs_state, and it restores ONLY containers"
    note "                whose restart policy is literally 'always'. The cloudflared tunnel is a"
    note "                separate unit with its own answer — see --tunnel-status."
    if [ -n "$STOPPED" ]; then
        note "stopped (kept for their logs; removed at the next launch):"
        while IFS='|' read -r id image name ports status; do
            [ -n "$id" ] || continue
            note "  $id  $image  name=$name  ports=$ports  $status"
        done <<< "$STOPPED"
    fi
    exit 0
fi

# --------------------------------------------------------------------------------------------
# The agent smoke test. THE ONLY ACTION HERE THAT COSTS MONEY.
#
# It is a separate script because it is a different kind of thing from everything else in this
# file: the rest of run.sh manages a container, this holds a real, billed conversation with a
# vendor. What run.sh contributes is the three facts it already knows and the operator would
# otherwise have to repeat — the URL, the channel, and which container holds the access log.
#
# Nothing here starts, stops, rebuilds or restarts anything. The container is READ from.
# --------------------------------------------------------------------------------------------

if [ "$SMOKE_AGENT" = 1 ]; then
    SMOKE_SCRIPT="$SCRIPT_DIR/smoke-agent.py"
    [ -x "$SMOKE_SCRIPT" ] || die "the smoke test is missing or not executable: $SMOKE_SCRIPT"

    for var in VIBE_TALK_READ_TOKEN VIBE_TALK_WRITE_TOKEN VIBE_TALK_CHANNELS; do
        [ -n "${!var:-}" ] || die "--smoke-agent needs $var, and it is not set.
It reads the channel with the read token and mints a signed conversation URL with the write one
(minting requires the write scope, because the conversation it opens reaches an agent that can
post). Set it in ${SOURCED_FILES[*]}."
    done

    # First configured channel: 'id:label:mode,id:label'. The smoke test asks about ONE channel,
    # and the first one is the one the owner listed first.
    SMOKE_CHANNEL="${VIBE_TALK_CHANNELS%%,*}"
    SMOKE_CHANNEL="${SMOKE_CHANNEL%%:*}"
    SMOKE_CHANNEL="$(printf '%s' "$SMOKE_CHANNEL" | tr -d '[:space:]')"
    [ -n "$SMOKE_CHANNEL" ] || die "could not read a channel snowflake out of VIBE_TALK_CHANNELS."

    # Read the log from the container that is actually serving, not from the name we would use if
    # we launched one. Those differ exactly when something unexpected is running — which is
    # precisely when reading the wrong log would mislead.
    if [ -z "$FOUND" ]; then
        die "nothing is running on ${HOST_ADDR}:${HOST_PORT} (image $IMAGE_REF), so there is no
live deployment to hold a conversation against and no access log to check the agent against.
Start it with: $SCRIPT_DIR/run.sh"
    fi
    IFS='|' read -r _smoke_id _smoke_image SMOKE_CONTAINER _smoke_ports _smoke_status <<< "$(head -1 <<< "$FOUND")"

    SMOKE_ARGS=(
        --url "http://${HOST_ADDR}:${HOST_PORT}"
        --channel "$SMOKE_CHANNEL"
        --container "$SMOKE_CONTAINER"
        --engine "$ENGINE"
    )
    if [ "$SMOKE_NONCE" = 1 ]; then SMOKE_ARGS+=(--nonce); fi
    if [ "$SMOKE_REPLAY" = 1 ]; then SMOKE_ARGS+=(--replay-check); fi

    step "Agent smoke test — a REAL conversation with the deployed agent. THIS COSTS VENDOR MINUTES."
    note "target:    http://${HOST_ADDR}:${HOST_PORT}"
    note "channel:   $SMOKE_CHANNEL"
    note "log from:  $SMOKE_CONTAINER  ($_smoke_status)"
    if [ "$SMOKE_REPLAY" = 1 ]; then
        note "mode:      --replay-check — THREE conversations: one to establish a fact, one opened"
        note "           with the record of it, and a CONTROL opened without. Nothing is posted to"
        note "           the channel; the transcript is written through this server's own API."
    fi
    if [ "$SMOKE_NONCE" = 1 ]; then
        note "mode:      --nonce — posts ONE unique token to the channel through this server's own"
        note "           write API (never through the agent) and requires it back verbatim."
    else
        note "mode:      default — nothing is written to the channel unless the check FAILS, in"
        note "           which case it escalates to the token round and holds a second (billed)"
        note "           conversation to find out whether the agent or this test is at fault."
    fi
    note "the agent is never asked to post; this reads."

    # Tokens go through the ENVIRONMENT, never the command line: a command line is readable by
    # every process on this box, and this script's standing promise is that it never puts a
    # credential anywhere it can be read.
    export VIBE_TALK_READ_TOKEN VIBE_TALK_WRITE_TOKEN
    # An `if` block rather than `[ ... ] && export ...`: under `set -e` a false test there is the
    # last command of the AND-list, and the script exits silently. Same trap as the one already
    # noted at the action-conflict check above.
    if [ -n "${VIBE_TALK_ELEVENLABS_API_KEY:-}" ]; then export VIBE_TALK_ELEVENLABS_API_KEY; fi

    if [ "$DRY_RUN" = 1 ]; then
        note "(dry run) $SMOKE_SCRIPT ${SMOKE_ARGS[*]}"
        note "(dry run) nothing was connected to and nothing was billed."
        exit 0
    fi
    exec "$SMOKE_SCRIPT" "${SMOKE_ARGS[@]}"
fi

# --------------------------------------------------------------------------------------------
# Logs. A stopped container still has them, which is the reason --rm is not used.
# --------------------------------------------------------------------------------------------

if [ "$LOGS_ONLY" = 1 ] || [ "$FOLLOW" = 1 ]; then
    target=""
    if [ -n "$FOUND" ]; then
        target="$(head -1 <<< "$FOUND")"
    else
        target="$(all_containers | head -1 || true)"
    fi
    if [ -z "${target%%|*}" ] || [ -z "$target" ]; then
        die "no vibe-talk container to read logs from.
Nothing matches image '$IMAGE_REF' or published host port $HOST_PORT, running or stopped.

If it was started before this script grew a fixed name, it may be under a different image tag:
  $ENGINE ps -a
Otherwise start it with: $SCRIPT_DIR/run.sh"
    fi
    IFS='|' read -r log_id _log_image log_name _log_ports log_status <<< "$target"
    if [ "$FOLLOW" = 1 ]; then
        step "Following $log_name ($log_id) — $log_status. Ctrl-C stops READING, not the container."
        if [ "$DRY_RUN" = 1 ]; then note "(dry run) $ENGINE logs -f $log_id"; exit 0; fi
        exec "$ENGINE" logs -f "$log_id"
    fi
    step "Logs for $log_name ($log_id) — $log_status"
    if [ "$DRY_RUN" = 1 ]; then note "(dry run) $ENGINE logs $log_id"; exit 0; fi
    exec "$ENGINE" logs "$log_id"
fi

if [ -n "$FOUND" ]; then
    if [ "$SHUTDOWN" = 1 ] || [ "$RESTART" = 1 ]; then
        stop_running "$FOUND"
        if [ "$SHUTDOWN" = 1 ]; then
            step "Stopped. Not rebuilding or relaunching (that is what --shutdown means; use --restart to stop and relaunch)."
            exit 0
        fi
    else
        echo "" >&2
        echo "vibe-talk is already running:" >&2
        while IFS='|' read -r id image name ports status; do
            [ -n "$id" ] || continue
            echo "  $id  $image  name=$name  ports=$ports  $status" >&2
        done <<< "$FOUND"
        echo "" >&2
        echo "Matched on image '$IMAGE_REF' or published host port $HOST_PORT." >&2
        echo "Re-run with --shutdown to stop it and exit, or --restart to stop it and relaunch." >&2
        exit 1
    fi
elif [ "$SHUTDOWN" = 1 ]; then
    step "Nothing to stop: no container matches image $IMAGE_REF or host port $HOST_PORT."
    exit 0
fi

# ---------------------------------------------------------------------------------------------
# 2 (second half). Validate that the required variables are actually SET.
#
# Checking only that the file exists is what lets a typo'd variable name through, and the symptom
# is a container that starts and then does not work.
# ---------------------------------------------------------------------------------------------

step "Configuration"
note "config file: ${SOURCED_FILES[*]}"

MISSING=()
PLACEHOLDER=()
for var in "${REQUIRED_VARS[@]}"; do
    value="${!var:-}"
    if [ -z "$value" ]; then
        MISSING+=("$var")
    elif [[ "$value" == *REPLACE-ME* ]]; then
        PLACEHOLDER+=("$var")
    fi
done

if [ "${#MISSING[@]}" -gt 0 ]; then
    die "required configuration variable(s) not set: ${MISSING[*]}

They were not found in: ${SOURCED_FILES[*]}
(nor in the environment). A file that exists but has a misspelled variable name looks fine and
produces a container that starts and then fails, which is exactly what this check is for.

Set each one in your config file. See $EXAMPLE_CONFIG for what each means."
fi

if [ "${#PLACEHOLDER[@]}" -gt 0 ]; then
    die "configuration variable(s) still hold the example placeholder: ${PLACEHOLDER[*]}
Replace the REPLACE-ME values with real ones in: ${SOURCED_FILES[*]}"
fi

# VIBE_TALK_CHANNELS is comma-separated `id:label:rw` (or `:ro`). It is the most typo-prone value
# here and a malformed entry is only discovered at server startup, so check the shape now.
IFS=',' read -r -a channel_entries <<< "$VIBE_TALK_CHANNELS"
for entry in "${channel_entries[@]}"; do
    [ -n "${entry// /}" ] || continue
    if ! [[ "$entry" =~ ^[[:space:]]*[0-9]+:[^:]*:(rw|ro)[[:space:]]*$ ]]; then
        die "VIBE_TALK_CHANNELS entry is malformed: '$entry'
Expected comma-separated entries of the form  <channel-id>:<label>:rw  (or :ro), for example
  VIBE_TALK_CHANNELS='123456789012345678:lead team:rw,987654321098765432:build noise:ro'"
    fi
done

# Report by NAME only. No value of any variable is ever printed by this script.
set_names=(); unset_names=()
for var in "${REQUIRED_VARS[@]}" "${OPTIONAL_VARS[@]}"; do
    if [ -n "${!var:-}" ]; then set_names+=("$var"); else unset_names+=("$var"); fi
done
note "set:     ${set_names[*]}"
note "not set: ${unset_names[*]:-(none)}"
note "channels configured: ${#channel_entries[@]}"

if [ -z "${VIBE_TALK_ELEVENLABS_API_KEY:-}" ] || [ -z "${VIBE_TALK_ELEVENLABS_AGENT_ID:-}" ]; then
    note "note: voice is OFF (needs both VIBE_TALK_ELEVENLABS_API_KEY and VIBE_TALK_ELEVENLABS_AGENT_ID)."
fi

# ---------------------------------------------------------------------------------------------
# 3. Tunnel check. Optional, default OFF.
# ---------------------------------------------------------------------------------------------

if [ "$TUNNEL_ENABLED" = 1 ]; then
    step "Tunnel: $TUNNEL_UNIT (systemctl --user)"
    command -v systemctl >/dev/null 2>&1 \
        || die "VIBE_TALK_TUNNEL_ENABLED is on but systemctl is not on PATH.
Set VIBE_TALK_TUNNEL_ENABLED=0 (or pass --no-tunnel) on a host without systemd."

    if ! systemctl --user list-unit-files --no-legend "$TUNNEL_UNIT" 2>/dev/null | grep -q . \
       && ! systemctl --user cat "$TUNNEL_UNIT" >/dev/null 2>&1; then
        die "the tunnel is enabled but the user unit '$TUNNEL_UNIT' does not exist.
Create it at ~/.config/systemd/user/$TUNNEL_UNIT (it runs 'cloudflared tunnel run <name>' and
reads ~/.cloudflared/config.yml), then:
  systemctl --user daemon-reload && systemctl --user enable --now $TUNNEL_UNIT
Or set VIBE_TALK_TUNNEL_ENABLED=0 / pass --no-tunnel to skip this check."
    fi

    if systemctl --user is-active --quiet "$TUNNEL_UNIT"; then
        note "already active."
    elif [ "$DRY_RUN" = 1 ]; then
        note "(dry run) would run: systemctl --user start $TUNNEL_UNIT"
    else
        note "not active; starting it."
        systemctl --user start "$TUNNEL_UNIT" \
            || die "'systemctl --user start $TUNNEL_UNIT' failed. Diagnose with:
  systemctl --user status $TUNNEL_UNIT
  journalctl --user -u $TUNNEL_UNIT -n 50"
        systemctl --user is-active --quiet "$TUNNEL_UNIT" \
            || die "$TUNNEL_UNIT was started but is not active. See: journalctl --user -u $TUNNEL_UNIT -n 50"
        note "started."
    fi

    if ! systemctl --user is-enabled --quiet "$TUNNEL_UNIT" 2>/dev/null; then
        note "warning: $TUNNEL_UNIT is not enabled, so it will not come back after a reboot."
        note "         fix with: systemctl --user enable $TUNNEL_UNIT (and: loginctl enable-linger \"\$USER\")"
    fi
fi

# ---------------------------------------------------------------------------------------------
# 4. Rebuild.
# ---------------------------------------------------------------------------------------------

step "Building $IMAGE_REF from $VIBE_TALK_DIR/Containerfile"
build_args=(build -t "$IMAGE_REF" -f "$VIBE_TALK_DIR/Containerfile")
[ -n "$BUILD_NETWORK" ] && build_args+=(--network "$BUILD_NETWORK")
build_args+=("$VIBE_TALK_DIR")
if [ "$DRY_RUN" = 1 ]; then
    note "(dry run) $ENGINE ${build_args[*]}"
else
    "$ENGINE" "${build_args[@]}" \
        || die "image build failed. Fix the build before relaunching; the previous image is
untouched. If it failed DOWNLOADING DEPENDENCIES rather than compiling them, the build's own
network namespace is the first thing to suspect, not the Containerfile: try --build-network host."
fi

# ---------------------------------------------------------------------------------------------
# 5. Relaunch.
#
# Every secret is passed as a bare `-e NAME`, which tells the engine to take the value from THIS
# process's environment. No credential is ever written onto a command line, so none of them show
# up in `ps`, in a shell history, or in this script's own output.
# ---------------------------------------------------------------------------------------------

# The leftover-container sweep happens HERE, as late as possible — after the build has succeeded
# — and not up in the detection section. Removing a stopped container destroys its logs, so if
# the run is going to abort for a bad config or a failed build, the evidence from the previous
# instance is still there to read.
STOPPED="$(stopped_containers || true)"
if [ -n "$STOPPED" ]; then
    remove_stopped "$STOPPED"
fi

# Narrow safety net, not the detection rule: a container could hold the fixed name without
# matching our image or port (someone launched one by hand, or the image was renamed). Without
# this, `run --name` fails with an engine-level error that says nothing about this script.
NAME_HOLDER="$("$ENGINE" ps -a --filter "name=^${CONTAINER_NAME}$" --format '{{.ID}}|{{.Image}}|{{.Status}}' 2>/dev/null | head -1 || true)"
if [ -n "$NAME_HOLDER" ]; then
    holder_id="${NAME_HOLDER%%|*}"
    if "$ENGINE" ps --filter "name=^${CONTAINER_NAME}$" --format '{{.ID}}' 2>/dev/null | grep -qxF -- "$holder_id"; then
        die "the name '$CONTAINER_NAME' is held by a RUNNING container that does not match
image '$IMAGE_REF' or host port $HOST_PORT:
  $NAME_HOLDER
Refusing to touch it — it is not ours to stop. Stop it yourself, or set
VIBE_TALK_CONTAINER_NAME to a different name."
    fi
    step "Removing stopped container '$CONTAINER_NAME' ($holder_id) that holds the name"
    if [ "$DRY_RUN" = 1 ]; then
        note "(dry run) $ENGINE rm $holder_id"
    else
        "$ENGINE" rm "$holder_id" >/dev/null \
            || die "failed to remove '$CONTAINER_NAME' ($holder_id). Remove it by hand: $ENGINE rm $holder_id"
    fi
fi

# -d and no --rm: see the header. The container outlives this shell, and keeps its logs when it
# stops.
# Create the state directory before the mount, and privately: podman would otherwise create it
# root-owned or with the umask's permissions, and what lands in it is the owner's speech and
# third-party channel text.
if [ "$DRY_RUN" = 1 ]; then
    note "(dry run) would ensure $DATA_DIR exists, mode 0700"
else
    mkdir -p -- "$DATA_DIR" || die "cannot create the state directory: $DATA_DIR
Set VIBE_TALK_DATA_DIR to somewhere this user can write."
    chmod 700 -- "$DATA_DIR" || die "cannot restrict the state directory: $DATA_DIR"
fi

# THE MOUNT ARGUMENTS, kept in one array because the write PROBE below has to use exactly the
# ones the launch uses. A probe that tested a different mount would certify nothing.
#
# :Z relabels for SELinux, which is what makes the mount readable to a rootless container on a
# host that enforces it. Without it the server starts and every storage call fails with EACCES.
#
# --userns=keep-id is the other half, and it is the one whose absence broke this outright. Under
# ROOTLESS podman the invoking user is uid 0 inside the container's user namespace, and every
# other uid maps into the subuid range — so $DATA_DIR, owned by this user and mode 0700, appears
# inside as root:root drwx------ and the image's `vibe` (uid 10001) cannot write to it. The server
# opens its SQLite file at startup and exits when it cannot, and the container is launched with a
# restart policy, so the whole deployment is a crash loop. `keep-id:uid=...,gid=...` maps the
# container's server user back onto this host user, which is what makes the owner's own directory
# writable by it. It is rootless-only: rootful podman and docker reject it, and there uid 10001
# in the container IS uid 10001 on the host, so the directory has to be owned accordingly — the
# probe below is what says so, by name, instead of leaving it to be discovered from a crash loop.
CONTAINER_UID=10001
CONTAINER_GID=10001
mount_args=(-v "${DATA_DIR}:${CONTAINER_DATA_DIR}:Z")
ROOTLESS="$("$ENGINE" info --format '{{.Host.Security.Rootless}}' 2>/dev/null || true)"
if [ "$ROOTLESS" = "true" ]; then
    mount_args+=("--userns=keep-id:uid=${CONTAINER_UID},gid=${CONTAINER_GID}")
fi

# The preflight. This script exists to make a bad start fail HERE rather than five minutes later
# inside a container, and "the store cannot be opened" is the one failure that takes the whole
# server down. So it is checked with the real image, the real mount arguments and the real
# container user, before anything is launched.
step "Checking $DATA_DIR is writable by the container's user (uid $CONTAINER_UID)"
if [ "$DRY_RUN" = 1 ]; then
    note "(dry run) would write a probe file to $CONTAINER_DATA_DIR as uid $CONTAINER_UID"
elif ! PROBE_OUTPUT="$("$ENGINE" run --rm "${mount_args[@]}" --entrypoint /bin/sh "$IMAGE_REF" \
        -c "probe=\"${CONTAINER_DATA_DIR}/.write-probe.\$\$\"; : > \"\$probe\" && rm -f \"\$probe\"" \
        2>&1)"; then
    # The engine's own words are reproduced, because "the mount is not writable" and "this image
    # has no /bin/sh to run the probe with" fail identically here and are fixed differently.
    die "the durable-state mount is NOT writable by the container's user (uid $CONTAINER_UID).
  host directory: $DATA_DIR
  mounted at:     $CONTAINER_DATA_DIR
  engine:         $ENGINE (rootless=${ROOTLESS:-unknown})
  $ENGINE said:   ${PROBE_OUTPUT:-(nothing)}
The server opens its SQLite file at startup and REFUSES to start when it cannot, and this
container is launched with --restart $RESTART_POLICY, so launching now would produce a crash
loop rather than a server.
Rootless podman: this needs --userns=keep-id, which this script adds automatically; if you are
seeing this anyway the engine rejected it — check '$ENGINE info'.
Rootful podman or docker: uid $CONTAINER_UID inside the container is uid $CONTAINER_UID on the
host, so either chown the directory to it (sudo chown $CONTAINER_UID:$CONTAINER_GID '$DATA_DIR')
or point VIBE_TALK_DATA_DIR at a directory that user can write."
fi

run_args=(run -d --name "$CONTAINER_NAME" --restart "$RESTART_POLICY" --log-driver "$LOG_DRIVER" \
    -p "${HOST_ADDR}:${HOST_PORT}:8080")
run_args+=("${mount_args[@]}")
for var in "${REQUIRED_VARS[@]}" "${OPTIONAL_VARS[@]}"; do
    [ -n "${!var:-}" ] || continue
    export "${var?}"
    run_args+=(-e "$var")
done
run_args+=("$IMAGE_REF")

step "Running $IMAGE_REF as '$CONTAINER_NAME' on ${HOST_ADDR}:${HOST_PORT} (detached, restart=$RESTART_POLICY)"
note "(secrets are passed by name from this process's environment, never on the command line)"
if [ "$DRY_RUN" = 1 ]; then
    note "(dry run) $ENGINE ${run_args[*]}"
    exit 0
fi

CONTAINER_ID="$("$ENGINE" "${run_args[@]}")" \
    || die "the container failed to start. The image built fine, so this is a runtime problem:
  $ENGINE logs $CONTAINER_NAME"

step "Started $CONTAINER_NAME (${CONTAINER_ID:0:12})"
note "follow the output:  $SCRIPT_DIR/run.sh --follow"
note "one-shot log dump:  $SCRIPT_DIR/run.sh --logs"
note "stop it:            $SCRIPT_DIR/run.sh --shutdown   (logs survive; they go at the next launch)"
note "state, any time:    $SCRIPT_DIR/run.sh --status"
note "durable state:      $DATA_DIR (mounted at $CONTAINER_DATA_DIR; survives rebuilds, delete it to purge)"
