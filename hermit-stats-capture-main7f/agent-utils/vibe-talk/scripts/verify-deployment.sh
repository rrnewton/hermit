#!/usr/bin/env bash
# Verify a running vibe-talk deployment end to end, over the MCP endpoint an ElevenLabs agent
# will actually use.
#
# Run this twice: once against http://127.0.0.1:8080 right after `podman run` (step 3 of
# QUICKSTART.md), and again against the public tunnel hostname once cloudflared is up (step 4).
# The same checks apply to both, which is the point: if the tunnel changed anything, the second
# run says so.
#
# It exits non-zero on the FIRST failure and names the check that failed. There is no partial
# success and no check that can silently no-op: every check either prints "ok" with what it saw,
# or stops the script.
#
# Usage:
#   scripts/verify-deployment.sh --url URL --channel SNOWFLAKE \
#       [--read-token T] [--write-token T] [--skip-post]
#
# Tokens default to $VIBE_TALK_READ_TOKEN / $VIBE_TALK_WRITE_TOKEN, so the same environment that
# started the container can drive the check.
#
# --skip-post drops the two checks that put a real message in a real channel. Use it when you are
# pointed at a channel you would rather not write to; understand that you are then NOT testing the
# half of the system that speaks in your name.

set -euo pipefail

URL=""
CHANNEL=""
READ_TOKEN="${VIBE_TALK_READ_TOKEN:-}"
WRITE_TOKEN="${VIBE_TALK_WRITE_TOKEN:-}"
SKIP_POST=0
# A snowflake that is syntactically valid and certainly not in anyone's allowlist. Used to prove
# the allowlist refuses an unconfigured channel rather than falling through to Discord.
UNCONFIGURED_CHANNEL="999999999999999999"

usage() {
    sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --url) URL="${2:?--url needs a value}"; shift 2 ;;
        --channel) CHANNEL="${2:?--channel needs a value}"; shift 2 ;;
        --read-token) READ_TOKEN="${2:?--read-token needs a value}"; shift 2 ;;
        --write-token) WRITE_TOKEN="${2:?--write-token needs a value}"; shift 2 ;;
        --unconfigured-channel) UNCONFIGURED_CHANNEL="${2:?needs a value}"; shift 2 ;;
        --skip-post) SKIP_POST=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unrecognized argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

fail() {
    echo "" >&2
    echo "FAILED: $*" >&2
    exit 1
}

[ -n "$URL" ] || fail "no --url. Pass http://127.0.0.1:8080 for the local check, or https://<your-tunnel-hostname> for the public one."
[ -n "$CHANNEL" ] || fail "no --channel. Pass one of the channel snowflake ids you put in VIBE_TALK_CHANNELS."
[ -n "$READ_TOKEN" ] || fail "no read token. Pass --read-token or export VIBE_TALK_READ_TOKEN."
if [ "$SKIP_POST" -eq 0 ] && [ -z "$WRITE_TOKEN" ]; then
    fail "no write token. Pass --write-token, export VIBE_TALK_WRITE_TOKEN, or pass --skip-post to leave the posting half untested."
fi
command -v curl >/dev/null 2>&1 || fail "curl is not installed; this script needs it."
command -v python3 >/dev/null 2>&1 || fail "python3 is not installed; this script uses it to read JSON."

URL="${URL%/}"

# ---------------------------------------------------------------------------
# plumbing
# ---------------------------------------------------------------------------

# Populated by `request`. BODY is the response body, STATUS the HTTP status code.
BODY=""
STATUS=""

# request METHOD PATH [AUTH_TOKEN] [JSON_BODY]
request() {
    local method="$1" path="$2" token="${3:-}" data="${4:-}"
    local args=(-s -S -o - -w '\n%{http_code}' -X "$method" --max-time 30 "$URL$path")
    [ -n "$token" ] && args+=(-H "authorization: Bearer $token")
    if [ -n "$data" ]; then
        args+=(-H 'content-type: application/json' -H 'accept: application/json' --data "$data")
    fi
    local raw
    if ! raw="$(curl "${args[@]}" 2>&1)"; then
        fail "could not reach $URL$path. curl said: $raw"
    fi
    STATUS="${raw##*$'\n'}"
    BODY="${raw%$'\n'*}"
}

rpc_id=0
# rpc TOKEN METHOD PARAMS_JSON
rpc() {
    rpc_id=$((rpc_id + 1))
    local token="$1" method="$2" params="${3:-}"
    [ -n "$params" ] || params='{}'
    request POST /mcp "$token" \
        "{\"jsonrpc\":\"2.0\",\"id\":$rpc_id,\"method\":\"$method\",\"params\":$params}"
}

# tool_call TOKEN TOOL_NAME ARGS_JSON
tool_call() {
    rpc "$1" tools/call "{\"name\":\"$2\",\"arguments\":$3}"
}

# The three JSON readers below are Python programs held in variables rather than heredocs,
# because a heredoc would occupy the same stdin the response body has to arrive on.

PY_JGET='
import json, sys
path = sys.argv[1]
try:
    doc = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for part in path.split("."):
    if not part:
        continue
    if isinstance(doc, list):
        try:
            doc = doc[int(part)]
        except (ValueError, IndexError):
            sys.exit(1)
    elif isinstance(doc, dict):
        if part not in doc:
            sys.exit(1)
        doc = doc[part]
    else:
        sys.exit(1)
print(doc if isinstance(doc, str) else json.dumps(doc))
'

PY_TOOL_NAMES='
import json, sys
try:
    tools = json.load(sys.stdin)["result"]["tools"]
except Exception:
    sys.exit(1)
for tool in tools:
    print(tool["name"])
'

# The commit body for the pending proposal in $BODY, restating exactly what the server holds. Fails
# unless that proposal is the one carrying this run's nonce, so a stale one is never confirmed.
PY_COMMIT_BODY='
import json, sys
try:
    pending = json.load(sys.stdin)["proposal"]
except Exception:
    sys.exit(1)
if not pending or sys.argv[1] not in pending["text"]:
    sys.exit(1)
keys = ("handle", "channel_id", "text", "reply_to")
print(json.dumps({**{k: pending[k] for k in keys}, "confirmed_by": "ui"}))
'

PY_TOOL_TEXT='
import json, sys
try:
    content = json.load(sys.stdin)["result"]["content"]
except Exception:
    sys.exit(1)
print("\n".join(c.get("text", "") for c in content))
'

# Read a dotted path out of $BODY. Prints the value; stops the script if it is not there, because
# a missing field means the response was not the shape we are asserting about.
jget() {
    python3 -c "$PY_JGET" "$1" <<<"$BODY" || fail "response had no field '$1'. Body was: $BODY"
}

# Tool names from a tools/list response, one per line.
tool_names() {
    python3 -c "$PY_TOOL_NAMES" <<<"$BODY" || fail "tools/list did not return a tool array. Body was: $BODY"
}

# The concatenated text of a tools/call result.
tool_text() {
    python3 -c "$PY_TOOL_TEXT" <<<"$BODY" || fail "tools/call did not return a content array. Body was: $BODY"
}

# The first 300 characters of $BODY. Used where a SUCCESSFUL response is being dumped as
# evidence of a failure, since those can be kilobytes of channel text.
body_excerpt() {
    printf '%.300s' "$BODY"
    [ "${#BODY}" -gt 300 ] && printf ' ... (%d bytes total)' "${#BODY}"
    printf '\n'
}

checks_run=0
ok() {
    checks_run=$((checks_run + 1))
    printf '  ok    %s\n' "$*"
}
checking() { printf '%s\n' "-- $*"; }

echo "vibe-talk deployment check"
echo "   target:  $URL"
echo "   channel: $CHANNEL"
[ "$SKIP_POST" -eq 1 ] && echo "   posting: SKIPPED (--skip-post) — the write half is NOT being tested"
echo ""

# ---------------------------------------------------------------------------
# 0. it is up at all
# ---------------------------------------------------------------------------
checking "the server is reachable"
request GET /healthz
[ "$STATUS" = "200" ] || fail "check 0 (reachable): GET /healthz returned $STATUS, expected 200. Nothing else below is meaningful until this passes. Is the container running, and is --url right?"
ok "GET /healthz → 200"

# ---------------------------------------------------------------------------
# 1. an unauthenticated MCP call is refused, and learns nothing
# ---------------------------------------------------------------------------
checking "an unauthenticated caller is refused"
request POST /mcp "" '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
[ "$STATUS" = "401" ] || fail "check 1 (unauthenticated): POST /mcp with no token returned $STATUS, expected 401. Your endpoint is answering strangers. Body was: $BODY"
ok "unauthenticated POST /mcp → 401"

for leak in post_reply digest_channel list_channels "$CHANNEL" vibe-talk 2025-06-18; do
    case "$BODY" in
        *"$leak"*) fail "check 1 (unauthenticated): the 401 body mentioned '$leak'. It is supposed to name no tool, channel, service, or protocol revision. Body was: $BODY" ;;
    esac
done
ok "the 401 body names no tool, channel, service, or protocol revision"

request GET /mcp "$READ_TOKEN"
[ "$STATUS" = "405" ] || fail "check 1 (unauthenticated): GET /mcp returned $STATUS, expected 405. This endpoint is stateless and must not hold a stream open."
ok "GET /mcp → 405 (stateless; no dangling SSE stream)"

# ---------------------------------------------------------------------------
# 2. the read token works, and is not shown post_reply
# ---------------------------------------------------------------------------
checking "the read token can list tools, and post_reply is not among them"
rpc "$READ_TOKEN" tools/list
[ "$STATUS" = "200" ] || fail "check 2 (read tools/list): returned $STATUS, expected 200. Is the read token exactly what the server was started with? Body was: $BODY"
read_tools="$(tool_names)"
for expected in recent_activity list_channels digest_channel read_page count_messages find_message read_message; do
    grep -qx "$expected" <<<"$read_tools" || fail "check 2 (read tools/list): '$expected' was missing from the read token's tool list. Tools offered: $(tr '\n' ' ' <<<"$read_tools")"
done
ok "read token sees recent_activity, list_channels, digest_channel, read_page, count_messages, find_message, read_message"

if grep -qx post_reply <<<"$read_tools"; then
    fail "check 2 (read tools/list): post_reply WAS offered to the read token. A read credential must not even see the posting tool. Tools offered: $(tr '\n' ' ' <<<"$read_tools")"
fi
ok "read token is NOT offered post_reply"

# ---------------------------------------------------------------------------
# 3. the read token cannot post even by calling the hidden tool by name
# ---------------------------------------------------------------------------
checking "the read token is refused when it calls post_reply anyway"
tool_call "$READ_TOKEN" post_reply \
    "{\"channel_id\":\"$CHANNEL\",\"text\":\"vibe-talk verification: this must never be posted\"}"
[ "$STATUS" = "403" ] || fail "check 3 (read cannot post): calling post_reply with the read token returned $STATUS, expected 403. THE READ TOKEN CAN POST — do not put it in an agent until this passes. Body was: $BODY"
code="$(jget error.code)"
[ "$code" = "-32001" ] || fail "check 3 (read cannot post): expected JSON-RPC error code -32001, got '$code'. Body was: $BODY"
ok "read token calling post_reply → HTTP 403, JSON-RPC -32001"

# ---------------------------------------------------------------------------
# 4. a channel outside the allowlist is refused
# ---------------------------------------------------------------------------
checking "a channel outside the allowlist is refused"
tool_call "$READ_TOKEN" digest_channel "{\"channel_id\":\"$UNCONFIGURED_CHANNEL\"}"
[ "$STATUS" = "200" ] || fail "check 4 (allowlist): expected HTTP 200 carrying a tool-level error, got $STATUS. Body was: $BODY"
is_error="$(jget result.isError)"
[ "$is_error" = "true" ] || fail "check 4 (allowlist): channel $UNCONFIGURED_CHANNEL was NOT refused — the allowlist is not holding. If that snowflake really is one of yours, re-run with --unconfigured-channel set to one that is not. Body began: $(body_excerpt)"
refusal="$(tool_text)"
case "$refusal" in
    *unknown_channel*) ;;
    *) fail "check 4 (allowlist): the refusal did not carry the unknown_channel code. It said: $refusal" ;;
esac
ok "unconfigured channel $UNCONFIGURED_CHANNEL → refused with unknown_channel"

# ---------------------------------------------------------------------------
# 5. the read token gets real messages back
# ---------------------------------------------------------------------------
checking "the read token gets real messages out of your channel"
tool_call "$READ_TOKEN" digest_channel "{\"channel_id\":\"$CHANNEL\",\"limit\":25}"
[ "$STATUS" = "200" ] || fail "check 5 (digest): returned $STATUS, expected 200. Body was: $BODY"
is_error="$(jget result.isError)"
digest="$(tool_text)"
if [ "$is_error" = "true" ]; then
    fail "check 5 (digest): the server refused to read channel $CHANNEL. It said: $digest
If this says unknown_channel, that snowflake is not in VIBE_TALK_CHANNELS. If it mentions Discord, the bot is probably not in that channel or lacks View Channel / Read Message History. If the digest is EMPTY rather than refused, check that Message Content Intent is enabled in the Discord Developer Portal — without it the bot receives every message with blank content."
fi
[ -n "$(tr -d '[:space:]' <<<"$digest")" ] || fail "check 5 (digest): the digest came back EMPTY. The most common cause by far is that Message Content Intent is NOT enabled for this bot in the Discord Developer Portal (Bot → Privileged Gateway Intents). Without it Discord delivers messages with blank content and everything downstream looks broken. Second most likely: the channel genuinely has no messages."
ok "digest_channel returned $(grep -c . <<<"$digest") non-empty lines of real channel content"
echo "        first line: $(head -n1 <<<"$digest" | cut -c1-100)"

# ---------------------------------------------------------------------------
# 6. the write token can post, and the post really lands in Discord
# ---------------------------------------------------------------------------
if [ "$SKIP_POST" -eq 1 ]; then
    echo ""
    echo "SKIPPED the two posting checks (--skip-post). The write path is UNVERIFIED."
else
    checking "the write token can post, and the message really lands in the channel"
    rpc "$WRITE_TOKEN" tools/list
    [ "$STATUS" = "200" ] || fail "check 6 (write tools/list): returned $STATUS, expected 200. Is the write token exactly what the server was started with? Body was: $BODY"
    grep -qx post_reply <<<"$(tool_names)" || fail "check 6 (write tools/list): the WRITE token was not offered post_reply. Check that this channel is configured 'rw' rather than 'ro'."
    ok "write token IS offered post_reply"

    # `#264 voice-send-approval`: whether post_reply proposes or posts is the owner's setting, so
    # read it before judging the answer. A server that does not report it always asks.
    request GET /api/v1/voice-settings "$READ_TOKEN"
    ask_before_send="true"
    if [ "$STATUS" = "200" ]; then
        ask_before_send="$(jget ask_before_send)"
    elif [ "$STATUS" != "404" ]; then
        fail "check 6 (voice settings): GET /api/v1/voice-settings returned $STATUS, expected 200. Body was: $BODY"
    fi

    nonce="gtverify-$(date +%s)-$$"
    tool_call "$WRITE_TOKEN" post_reply \
        "{\"channel_id\":\"$CHANNEL\",\"text\":\"vibe-talk deployment check $nonce — this message was posted by the verification script and can be deleted.\"}"
    [ "$STATUS" = "200" ] || fail "check 6 (propose): post_reply returned HTTP $STATUS, expected 200. Body was: $BODY"
    is_error="$(jget result.isError)"
    proposed="$(tool_text)"
    [ "$is_error" = "false" ] || fail "check 6 (propose): the proposal was refused. The server said: $proposed
If this mentions channel_not_writable, that channel is configured 'ro' — change it to 'rw'."
    if [ "$ask_before_send" = "false" ]; then
        # The owner turned confirmation off: post_reply posts at once, and must say so.
        case "$proposed" in
            "SENT."*) ;;
            *) fail "check 6 (direct send): \"Ask me before the voice agent sends\" is off, so post_reply should have posted and said SENT. It said: $proposed" ;;
        esac
        ok "post_reply posted at once and said so: $(head -n1 <<<"$proposed" | cut -c1-100)"
        echo ""
        echo "  NOTE: DIRECT SENDS ARE ON. \"Ask me before the voice agent sends\" is turned off on this"
        echo "  server, so the voice agent posts in your name as soon as it decides to, with no"
        echo "  confirmation. Tick it again in Settings if that is not what you want."
        echo ""
    else
        # `#34 voice-chat-write-confirm`: the model-facing tool only PROPOSES. A server that posted
        # here would be one whose agent can speak in your name without you.
        case "$proposed" in
            "NOT SENT."*) ;;
            *) fail "check 6 (propose): post_reply did not say NOT SENT. This server lets the model post without a separate confirmation. It said: $proposed" ;;
        esac
        ok "post_reply proposed and did not send: $(head -n1 <<<"$proposed" | cut -c1-100)"

        # The confirming half, as the voice page's Send does it: read the server's copy of the
        # pending proposal and restate it with its handle. This script is the confirming party
        # here — running it with a write token and without --skip-post is the decision to post.
        checking "the proposal is confirmed outside the model's tools, and only then posted"
        request GET /api/v1/post-proposals "$WRITE_TOKEN"
        [ "$STATUS" = "200" ] || fail "check 6 (pending): GET /api/v1/post-proposals returned $STATUS, expected 200. Body was: $BODY"
        commit="$(python3 -c "$PY_COMMIT_BODY" "$nonce" <<<"$BODY")" \
            || fail "check 6 (pending): the pending proposal is not the one this script just made. Body was: $BODY"
        request POST /api/v1/post-proposals/commit "$WRITE_TOKEN" "$commit"
        [ "$STATUS" = "200" ] || fail "check 6 (commit): confirming the proposal returned HTTP $STATUS, expected 200. If this is 502, the bot most likely lacks Send Messages in that channel. Body was: $BODY"
        ok "confirmed proposal $(jget serial) posted as message $(jget posted.id)"
    fi

    # Reading it back is what proves it reached Discord rather than being accepted and dropped.
    checking "the posted message is readable back out of Discord"
    tool_call "$READ_TOKEN" find_message "{\"channel_id\":\"$CHANNEL\",\"query\":\"$nonce\"}"
    [ "$STATUS" = "200" ] || fail "check 7 (read-back): find_message returned $STATUS. Body was: $BODY"
    found="$(tool_text)"
    case "$found" in
        *"$nonce"*) ;;
        *) fail "check 7 (read-back): the message this script just posted could not be read back out of channel $CHANNEL. the confirmed commit reported success, so it was accepted — but it is not in the window this server fetches. Look at the channel in Discord yourself before trusting the write path. find_message said: $found" ;;
    esac
    ok "the posted message came back out of Discord (nonce $nonce)"
    echo ""
    echo "Look at the channel in Discord now: you should see a message containing $nonce."
fi

# ---------------------------------------------------------------------------
# 8. the signed-URL route is not open to strangers
# ---------------------------------------------------------------------------
# This route MINTS a credential: a working conversation with your agent. An open one would be
# strictly worse than the public talk-to link that enabling agent authentication just closed. Both
# checks are decided by this server's own auth, before ElevenLabs is contacted, so they are
# meaningful whether or not the ElevenLabs half is configured yet.
checking "the signed-URL route refuses strangers and refuses the read token"
request GET /api/v1/signed-url ""
[ "$STATUS" = "401" ] || fail "check 8 (signed-url): GET /api/v1/signed-url with no token returned $STATUS, expected 401. ANYONE ON THE INTERNET CAN START A CONVERSATION WITH YOUR AGENT. Body was: $BODY"
ok "unauthenticated GET /api/v1/signed-url → 401"

request GET /api/v1/signed-url "$READ_TOKEN"
[ "$STATUS" = "403" ] || fail "check 8 (signed-url): the READ token got $STATUS from /api/v1/signed-url, expected 403. Minting a conversation is gated on the write scope, because the agent on the far end can post. Body was: $BODY"
ok "read token GET /api/v1/signed-url → 403"

echo ""
echo "PASSED — $checks_run checks against $URL"
echo ""
echo "What this did NOT check, and cannot:"
echo "  * Who confirms a proposal in your deployment. While \"Ask me before the voice agent sends\" is"
echo "    on, this server refuses to let post_reply send and holds each proposal for a separate"
echo "    confirmation (the voice page's Send, or a plain yes the voice page judges); this script"
echo "    confirmed its own. Whether a bridge of yours only confirms on a real yes is a property of"
echo "    that bridge, and nothing here can see it. What this proved is the half we do enforce: the"
echo "    read token cannot post, post_reply sends only as the owner's setting allows, and no token"
echo "    can reach a channel outside the allowlist."
echo "  * TLS, if you pointed this at http://. A tunnel gives you TLS; it does not authorize anyone."
