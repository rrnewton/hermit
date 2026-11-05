# vibe-talk quickstart: from nothing to a talking agent

Six steps, in order, with the exact commands. Nothing here asks you to read source or invent a
value. `README.md` is the reference; this is the path through it.

**It assumes you have nothing.** No cloud account, no domain, no reverse proxy, no orchestrator —
one machine that can run a process and reach `discord.com` outbound is the whole requirement.
**Step 4, putting it on the internet, is OPTIONAL**: a deployment reached only over your LAN or
over `localhost` is a complete one, and steps 5 and 6 are only for conversational voice.

For message read-aloud without a hosted agent, add this to the server's TOML configuration:

```toml
[read_aloud]
backend = "browser"
```

Restart and reload the page in Chrome on Android. Open the message view, press **Read**, and tap
a message. This uses the phone's configured speech engine and needs an installed voice.
See [device speech](README.md#reading-messages-with-a-device-voice) for setup,
offline-voice considerations, browser limitations, and the opt-in rendered-audio regression check.
Existing deployments keep ElevenLabs read-aloud unless they select this backend.

To install the reader on Android, serve it over HTTPS, open `/voice` in Chrome, then choose
**Install app** from the browser menu. Chrome opens the installed app in its own window. On iOS,
Safari's **Add to Home Screen** remains a browser-mode shortcut so microphone handling retains the
safer browser fallback. The app has no service worker or offline cache; private API responses are
`Cache-Control: no-store`. Only the page's own public files are cached, its scripts, styles and
icons by content hash, so a relaunch downloads only what a deploy changed. The channel view keeps a
bounded snapshot of recent rows in `localStorage`, cleared on sign-out or a token change, so a
revisit draws immediately (see [saved messages](README.md#saved-messages-on-this-device)). Remove
and reinstall an older shortcut if its icon stays blank.

Budget about an hour if you do all six, most of it clicking through two dashboards and waiting on
DNS. Half that if you stop after step 3.

**What you will have to go and get**, in the order you need it — collect it as you go, and note
that two of them (1 and 5) are shown exactly once and cannot be read back:

| # | Value | Where it comes from |
|---|---|---|
| 1 | Discord bot token | Developer Portal → Bot → Reset Token (step 1) |
| 2 | Channel snowflake ids | Discord → Developer Mode → right-click channel (step 1.7) |
| 3 | Your own Discord user id | Discord → right-click your own name → Copy User ID (step 1.8) |
| 4 | A read token and a write token | `openssl rand -base64 33`, twice (step 2) |
| 5 | ElevenLabs API key | elevenlabs.io → Settings → API Keys (step 5) |
| 6 | ElevenLabs agent id | the agent's own dashboard URL (step 5) |
| 7 | *(optional)* an ElevenLabs voice id | only if read-aloud should not use the agent's voice (step 5) |

## Read this before you start

Two of these six steps are **first contact with a vendor**, and you should know which ones, so a
failure there reads as expected rather than as a broken build:

* **Step 3 is very nearly the first time this project's Discord client has run against live
  Discord.** Every part of it is unit-tested and the whole server is end-to-end tested against an
  in-memory fake. Exactly one path has touched `discord.com`: the startup probe was run with a
  deliberately invalid bot token, and Discord's 401 was classified and reported correctly. No
  authenticated read and no post has ever reached the real API. If something breaks, step 3 is the
  likeliest place, and it is not a regression — it is the untested seam finally being tested.
* **Step 5 has never been completed by anyone.** No ElevenLabs agent has ever connected to this
  server. The registration values in step 5 come from ElevenLabs' documentation, not from a round
  trip that someone watched work. The MCP endpoint itself is tested — against the protocol and
  against `curl` — but the pairing is not.

Everything before and between those two — the server, the tokens, the allowlist, the scope split,
the container, the ingress — is tested and has been run.

**One blocker to rule out before you spend the hour:** MCP integrations are **unavailable on
ElevenLabs workspaces in Zero Retention Mode, and on HIPAA-enabled workspaces.** If your workspace
is either, the voice half of this path is closed — the custom-MCP-server option will not be there
to click in step 5, and there is no workaround. Steps 1 through 4 are unaffected. Check that
first, in the ElevenLabs workspace settings.

---

## Step 1 — Create the Discord bot

Use a **second, dedicated bot**, not the one your coding agents post with. This one is held by a
server that may end up reachable from the public internet.

1. Go to <https://discord.com/developers/applications> → **New Application**. Name it anything.
2. **Bot** tab → **Reset Token** → copy the token. This is `VIBE_TALK_DISCORD_BOT_TOKEN`.
   It is shown once. Put it somewhere before you navigate away.
3. **Bot** tab → **Privileged Gateway Intents** → turn on **MESSAGE CONTENT INTENT** → **Save
   Changes**.

> ### ⚠️ Do not skip the Message Content Intent
>
> This is one toggle, it is off by default, and it is the single most confusing thing that can go
> wrong in this whole setup. Without it a bot is delivered messages whose `content` field is
> **empty** — so the connection works, the channel is found, the message count is right, and every
> digest line is blank. Nothing reports an error. It looks like the summarizer is broken, or the
> channel is empty, or this server is broken. It is none of those.
>
> Honest scope note: this server reads over the Discord **REST** API
> (`GET /api/v10/channels/{id}/messages`), and the well-documented blank-content behaviour is on
> the **gateway** event path. We have not run against live Discord and therefore cannot tell you
> from our own experience whether REST is affected on your account. Enabling it costs one click
> and closes off the question; leaving it off means that if content does come back blank you will
> spend an hour before you suspect the right thing. Turn it on.

4. **OAuth2** → **URL Generator**:
   * Scopes: **`bot`**
   * Bot permissions: **View Channels**, **Read Message History**, and — only if you want the
     agent to be able to post — **Send Messages**
5. Open the generated URL, pick your server, authorize.
6. **Add the bot to each channel you care about. This is a separate step from step 5.**

   > ### ⚠️ Authorizing the invite adds the bot to the SERVER, not to the CHANNEL
   >
   > A **private** channel does not inherit it. Open the channel → **Edit Channel** →
   > **Permissions** → add the bot (or a role it has) → make sure **View Channel** and **Read
   > Message History** are allowed there. Server-wide permissions do not reach into a private
   > channel.
   >
   > This is the step that actually gets skipped. It was skipped here: the server was configured,
   > it started cleanly, it listed its channels, and the bot had never been added — which only
   > showed up later as empty results that looked like a bug in the code.
   >
   > **You no longer have to remember it.** As of the startup channel probe, the server reads one
   > message from every configured channel at startup and **refuses to start** if it cannot,
   > naming the channel and what to fix. Step 3 is where you will see that.
7. Get the channel ids: Discord → **User Settings → Advanced → Developer Mode** on, then
   right-click each channel → **Copy Channel ID**. These are the 17–20 digit snowflakes. Give each
   one a label you can say out loud — it is what you will use in a sentence to a voice agent.
8. Get **your own** user id, while Developer Mode is on: right-click **your own name** on any
   message → **Copy User ID**. That is `VIBE_TALK_DISCORD_OWNER_USER_ID` /
   `discord.owner_user_id`.

   > **This one cannot be derived from anything the server holds**, which is why it is a setting.
   > A bot token's first segment *is* the bot's user id, so messages this bridge posts for you are
   > recognised for free — but a bot account has no relationship to the person reading the channel.
   > Set this and the messages **you** typed into Discord yourself are drawn as yours; leave it
   > unset and they arrive looking like a third party's. It is not a secret: a user id rides on
   > every message that account has ever sent. The web app's Settings screen can also assign it
   > per account, in the browser, with no restart.

## Step 2 — Generate this server's own two tokens

These are **not** Discord's and **not** ElevenLabs'. They are the credentials this server demands
of its own callers, and they are what stands between the public internet and your Discord.

```sh
openssl rand -base64 33   # read token  — goes in your phone and in the voice agent
openssl rand -base64 33   # write token — the capability to post as your bot
```

They **must differ** and each must be **at least 24 characters**, or the server refuses to start
rather than coming up in a weaker configuration than you meant. That is a startup failure with a
clear message, not a silent downgrade.

The read token cannot post. That is enforced in this server, not by convention: a read credential
is not even shown the posting tool, and is refused with HTTP 403 if it calls it by name. So the
conservative first deployment gives ElevenLabs the **read** token.

## Step 3 — Build and run it, against real Discord for the first time

The commands below are spelled out so you can see what each piece does. Once you have done it
once, `scripts/run.sh` is the repeatable version of this whole step: it refuses to start on top
of an already-running instance, checks that every required variable is really set before it
builds anything, optionally makes sure the cloudflared tunnel unit is up, then rebuilds and
relaunches. Copy `vibe-talk.env.example` to `~/.config/vibe-talk/env` to configure it — that path
is outside every checkout, so the file cannot be committed by accident — and see
`scripts/run.sh --help` for the flags and the configuration precedence.

It launches the container **detached**, under the fixed name `vibe-talk`, and without `--rm`, so
the server does not die with the terminal that started it and its log survives a crash instead of
being destroyed by it. Day to day that gives you four commands:

```sh
scripts/run.sh --follow          # a tab that watches the server, as if you had run it yourself
scripts/run.sh --logs            # one-shot dump; works on a stopped container too
scripts/run.sh --status          # what is running, under what restart policy, with logs kept?
scripts/run.sh --tunnel-status   # the cloudflared unit: active, since when, which hostname
```

`--tunnel-status` only reports; it never starts or stops anything. Logs are kept when the
container stops and are discarded at the next launch, so read them before you relaunch.

For server-audio read-aloud, a completed browser start writes `read-aloud reached audible
playback` with one opaque `observation` and millisecond fields. `message_preparation` normally
happens when Read is enabled, before the tap. The on-tap server phases, `request_connection`,
`request_session_start`, `yield` (waiting for an earlier read to finish or acknowledge an
interrupt) and `first_audio`, add up to `request_to_first_audio`. `transfer` is the rest of the
browser's wait for its first media frame, and `browser_playback` runs from that frame to the
`playing` event. `tap_to_audible` is measured directly. Conversational reads also carry `session`
(which provider connection, counting from 1), `turn` (the prompt's number on it) and
`session_reused`, so a reused session is stated, not inferred from a zero. These records contain
no message text, credentials, provider identity, or deployment address. If the browser never
reaches `playing`, the earlier `read-aloud audio reached the server` record still shows the server
phases under the same observation id, and `read-aloud turn ended` shows whether a turn was
interrupted.

A conversational session is shared by every read and kept for five idle minutes. Enabling Read
refreshes that and replaces a session the provider has closed. Tapping a different message while
one is still being generated interrupts it and starts the new one on the same session; the parts
of one combined row play in order. A session whose provider reports an error, closes, does not
acknowledge an interrupt within three seconds, or returns no audio within 15 seconds of a prompt
is replaced, and a read that heard nothing is retried once, so an unanswered read fails after at
most two such waits rather than hanging.

```sh
cd vibe-talk
podman build -t vibe-talk:v0 -f Containerfile .
```

Export what you collected. The `VIBE_TALK_CHANNELS` format is `id:label:rw` or `id:label:ro`,
comma separated — the label is what you will say out loud, and `ro` means the agent can read that
channel but never post to it.

```sh
export VIBE_TALK_DISCORD_BOT_TOKEN='...'          # from step 1
export VIBE_TALK_DISCORD_OWNER_USER_ID='...'      # from step 1.8 — your own user id
export VIBE_TALK_READ_TOKEN='...'                 # from step 2
export VIBE_TALK_WRITE_TOKEN='...'                # from step 2
export VIBE_TALK_CHANNELS='123456789012345678:lead team:rw,987654321098765432:build noise:ro'
```

Run it bound to **loopback only**, so the only way in is whatever you deliberately put in front of
it — not your LAN, and not anything else on the host's network:

```sh
podman run -d --rm --name vibe-talk -p 127.0.0.1:8080:8080 \
  -e VIBE_TALK_DISCORD_BOT_TOKEN -e VIBE_TALK_DISCORD_OWNER_USER_ID \
  -e VIBE_TALK_READ_TOKEN -e VIBE_TALK_WRITE_TOKEN -e VIBE_TALK_CHANNELS \
  vibe-talk:v0
```

If you want to reach it from another device on your own network and stop there — which is a
perfectly good place to stop — publish on the LAN instead (`-p 8080:8080`) and skip step 4
entirely. Note that a browser will not give the page a microphone over a plain-HTTP LAN address:
`getUserMedia` needs a secure context, and only HTTPS and `localhost` count as one.

**Watch the first few lines of the log.** Before it binds a port, the server reads one message
from each configured channel and prints a line per channel:

```text
startup channel probe (a one-message read per configured channel):
  [ok] channel 123456789012345678 (lead team): readable
  [ok] channel 987654321098765432 (build noise): readable
```

If a channel is not readable the server **refuses to start**, exits non-zero, and tells you which
one and what to do — the bot is not in the server, or it cannot view this channel, or the
snowflake is wrong, or the token is bad. Those have different fixes, so it does not collapse them
into "unreachable". A rejected **token** stops the probe after the first channel, since every
other channel would only repeat the same 401.

It only ever **reads** — never posts, not even to a channel you marked `rw`.

One case is a loud warning rather than a refusal: if the read succeeds but every message comes
back with **empty content**, that is the Message Content Intent from step 1.3. It is not treated
as fatal because a message can genuinely be attachment-only, but it is called out by name, because
it is the one failure Discord reports as a success.

To start without the check — offline development against `--fake-discord`, or if you are certain
the check is wrong — use `--skip-startup-probe` or `VIBE_TALK_SKIP_STARTUP_PROBE=1`. The skip is
logged loudly; you will not skip it by accident.

Now run the deployment check. This is the one command that tells you whether step 1 through step 3
actually worked:

```sh
scripts/verify-deployment.sh \
  --url http://127.0.0.1:8080 \
  --channel 123456789012345678
```

It reads the two tokens out of the environment you just exported. It checks, in order, that an
unauthenticated call is refused with 401 and told nothing; that the read token can list tools and
is **not** offered `post_reply`; that the read token is refused with 403 if it calls `post_reply`
anyway; that a channel outside your allowlist is refused; that the read token gets your **real
messages** back; that the write token's `post_reply` only *proposes* (it must answer `NOT SENT`);
and that once the script confirms that proposal, as the voice page's Send button does, the message
can be read back out of Discord.

It exits non-zero on the first failure and names the check that failed. There is no partial pass.
If the digest comes back empty rather than refused, it says so and points you at the Message
Content Intent.

**Add `--skip-post` if you do not want a test message in that channel** — but then you have not
tested the half of the system that speaks in your name. Without it, the script posts one message
containing a unique marker and tells you the marker; look at the channel and confirm you see it.

> If you would rather see the web app before involving Discord at all, run the binary with
> `--fake-discord`: it serves an in-memory channel seeded with a realistic backlog, warns loudly on
> every start, and touches nothing real. The same verification script works against it.

## Step 4 — OPTIONAL: put it on the internet

**Skip this if you do not need it.** LAN-only and `localhost`-only are complete, supported
deployments: the Discord half, the web app, the digest, the scrollback and the verification script
all work with no ingress at all. You need this step only to reach the server from outside your own
network — and, specifically, if you want a **hosted** voice agent, because ElevenLabs calls the
MCP endpoint machine-to-machine from its own infrastructure and must be able to reach you.

**Any way in is fine, and vibe-talk has no opinion about which.** What it actually requires is:

* a URL a browser can load;
* **HTTPS if you want the microphone in the browser** — `getUserMedia` needs a secure context, and
  `localhost` counts as one, so a purely local setup needs no certificate;
* something else terminating TLS, because **vibe-talk terminates none itself**.

Any of these satisfies that: an SSH reverse tunnel to a host you already have; a reverse proxy
with a certificate (Caddy, nginx, Traefik); a tunnel service (Cloudflare Tunnel, ngrok, Tailscale
Funnel); a cloud load balancer; or a VPN / overlay network (WireGuard, Tailscale, ZeroTier) if you
only ever want your own devices to reach it. Use whichever you already run.

**What follows is one worked example, not a recommendation.** It is written out in full because a
complete recipe is more useful than six sketches. If you already know how you want to do this, do
that instead and rejoin at the verification command below.

### Worked example: a Cloudflare Tunnel

The server speaks plain HTTP and holds a bot token, so it must never be exposed directly. A tunnel
gives it a public HTTPS hostname with **no inbound port open**: `cloudflared` dials out, and
Cloudflare terminates TLS.

You need a domain on Cloudflare. The hostname can be a subdomain of one you already have.

```sh
cloudflared tunnel login                                    # opens a browser; pick your zone
cloudflared tunnel create vibe-talk                         # prints a tunnel UUID and a JSON path
cloudflared tunnel route dns vibe-talk vibetalk.dev
```

`tunnel create` prints two things you need for the next file: the **tunnel UUID** and the path to
the **credentials JSON** it just wrote (normally `~/.cloudflared/<uuid>.json`).

Write `~/.cloudflared/config.yml`:

```yaml
tunnel: 6f2a1c30-1111-2222-3333-444455556666
credentials-file: /home/you/.cloudflared/6f2a1c30-1111-2222-3333-444455556666.json

ingress:
  - hostname: vibetalk.dev
    service: http://127.0.0.1:8080
  # A catch-all is REQUIRED and must be last. cloudflared refuses to start without it.
  - service: http_status:404
```

Substitute your own UUID, path, and hostname. `tunnel:` accepts the tunnel's name as well, but the
UUID is what `create` gave you and it cannot be ambiguous.

Run it in the foreground first, so you can see it connect:

```sh
cloudflared tunnel run vibe-talk
```

Once it is good, install it as a service so it survives a reboot:

```sh
sudo cloudflared service install
sudo systemctl status cloudflared
```

### Verify the public URL — whichever way you exposed it

This part is not specific to the example above. Run the **same script** against your public URL,
which is the point of it taking one:

```sh
scripts/verify-deployment.sh \
  --url https://vibetalk.dev \
  --channel 123456789012345678
```

If this passes and step 3 passed, your ingress changed nothing, which is what you want to know. If
it fails where step 3 passed, the fault is in the thing you just put in front — not in the server.

### ⚠️ Do not put a browser login in front of this

Whatever you chose, do not fence `/mcp` with something that expects a human: an SSO or OAuth login
wall, an nginx `auth_request` against an identity provider, a load balancer's OIDC action, or —
the specific trap on the path above — **Cloudflare Access**.

Access is the natural next thought and it is the wrong move here. **Access expects a human with a
browser**: it answers an unauthenticated request with a login redirect. ElevenLabs calls this
endpoint machine-to-machine, with no browser and nobody to log in — so Access will bounce every
call, and the failure surfaces inside ElevenLabs as an unhelpful connection error rather than as
"you have an identity gate in the way".

If you want Access anyway, it has to be a **service token**: create one in Zero Trust → Access →
Service Auth, add a policy that allows it on this hostname, and add the two headers to the
ElevenLabs MCP server configuration alongside the `Authorization` header:

```
CF-Access-Client-Id:     <client id>.access
CF-Access-Client-Secret: <client secret>
```

**Transport is not authorization.** A tunnel, a proxy or a load balancer gives you TLS and hides
your host's address. None of them decides who may call. The thing deciding who may call is this
server's bearer token, and that is the part this codebase actually tests. If you do want a second
fence in front, it has to be one a machine can satisfy with a header it was configured with.

## Step 5 — Create the ElevenLabs agent and register the MCP server

Keep ElevenLabs' hosted LLM. Nothing in this project needs you to bring your own model.

1. Create the agent: <https://elevenlabs.io/app/agents> → new agent. A blank/default template is
   fine. Open it, and **the agent id is in the dashboard URL** — it starts `agent_`. That is
   `VIBE_TALK_ELEVENLABS_AGENT_ID`, and it is public: it identifies a widget.
2. Create the API key: <https://elevenlabs.io/app/settings/api-keys> → **Create API Key** → copy
   it. **It is shown once.** That is `VIBE_TALK_ELEVENLABS_API_KEY`, and it is used for two things:
   minting the short-lived signed URLs that open a conversation with an authenticated agent, and
   reading messages aloud. Unlike the agent id it is an **account** secret that can spend money, so
   it never leaves the server — it travels to ElevenLabs in a header, never in a URL, and is
   redacted out of every error message.

   > **You do not need a voice id.** `VIBE_TALK_ELEVENLABS_VOICE_ID` is optional and most
   > deployments should leave it unset: with it unset, read-aloud borrows the configured **agent's
   > own voice**, and the agent's delivery with it — speed, stability — so a channel message is
   > read out by the same voice that talks to you, from the one thing you already configured. Set
   > it only when those two should deliberately differ.

3. Register the MCP server. In the dashboard this is under the agent's integrations, or under
   **Conversational AI → MCP Servers** and then attached to the agent, depending on which
   navigation your account shows.

| Field | Value |
|---|---|
| Server URL | `https://vibetalk.dev/mcp` — your own public hostname, and the path **must** be `/mcp` |
| Transport | **Streamable HTTP** |
| Authentication | Header `Authorization` with value `Bearer <your read token>` |
| Approval mode | **Fine-Grained Tool Approval** |

**Transport must be Streamable HTTP.** If the dashboard also offers SSE, do not pick it. SSE is
MCP's legacy remote transport, superseded by Streamable HTTP in protocol revision `2025-03-26`,
and this server deliberately implements only the current one. Choosing SSE will fail to connect.

4. Set per-tool approval. All eight tools should appear once the server connects; if only seven
   do, you gave it the read token, which is the conservative choice and means posting is off
   entirely.

| Tool | Approval | Why |
|---|---|---|
| `recent_activity` | **auto** — no approval | "What happened today?" — every channel's last few hours in one call. |
| `list_channels` | **auto** — no approval | Names your channels. Reads nothing from Discord. |
| `digest_channel` | **auto** — no approval | One spoken line per recent message. This is the main one. |
| `read_page` | **auto** — no approval | Step further back, ten at a time, or jump to a period or the last few hours. Says it is a page. |
| `count_messages` | **auto** — no approval | "How many are in there?" — bounded, and says when it is a floor. |
| `find_message` | **auto** — no approval | "The one about the mac runner" → that message, in full. |
| `read_message` | **auto** — no approval | One known message by id. |
| **`post_reply`** | **REQUIRE APPROVAL** — or **auto** if you turned off "Ask me before the voice agent sends" | **Speaks in your name, in your channel.** |

That table is the whole policy: **reading is automatic while you are driving, posting asks first.**
The one exception is yours to make: the voice page's Settings has **Ask me before the voice agent
sends** first in the **Voice agent** section, under **Sending**, on by default. Turn it off and the agent posts as soon as you ask, with no
read-back — then set `post_reply` to **auto** here and paste the matching prompt in step 5 (no
re-paste of the prompt if you let the server give the agent its prompt, at the end of step 5; the
approval here is changed by hand either way).

**Be clear about who enforces that.** Fine-Grained Tool Approval runs on **ElevenLabs' side**.
This server cannot see it, cannot verify you set it, and cannot tell if it is changed later. What
this server enforces — and what the step 3 script proves — is different: a read token **cannot**
post, no token can reach a channel outside your allowlist, and — while "Ask me before the voice
agent sends" is on — even with the write token `post_reply` never sends by itself. It proposes, and
the post happens only when you tap **Send** on the card the voice page shows, or answer the agent's
"Send it?" with a plain yes that the voice page itself judges (see "Posting asks first, unless you
turn that off" in the README). If you want posting off entirely, give the agent the **read token**
and let it be physically incapable of proposing at all.

5. Give the agent the canonical system prompt: the whole of
   [`prompts/voice-agent-system.txt`](prompts/voice-agent-system.txt), then a blank line, then the
   section that matches the token you gave it —
   [`prompts/voice-agent-read-only.txt`](prompts/voice-agent-read-only.txt) for the read token;
   for the write token, [`prompts/voice-agent-send.txt`](prompts/voice-agent-send.txt) while "Ask
   me before the voice agent sends" is on (the default), or
   [`prompts/voice-agent-send-direct.txt`](prompts/voice-agent-send-direct.txt) if you turned it
   off. Paste that into the agent's system-prompt field. **Match the section to the token and the
   setting**: an agent told it may send while holding a read token promises sends it cannot make,
   and one given the wrong send rules asks "Send it?" when nobody will confirm, or says "sent" about
   a draft still waiting. Called with the same token, `GET /api/v1/voice-agent` returns the
   already-assembled text as `prompt.text`, and its `variant` names which one it is. **This agent
   was configured by hand, so it does not follow a change of the setting: paste the prompt again,
   and the `post_reply` approval from step 4, whenever you change it** — or let the server give it
   its prompt, below, and change only the approval. The files are
   provider-neutral so a deployment-managed voice bridge and a hosted agent can use the same
   reviewed instructions instead of maintaining separate, drifting copies.

   **Optional: let the server give the agent its prompt.** If the agent's **Security** settings
   allow the **system prompt** to be overridden, the voice page hands it this server's whole
   assembled prompt as every call starts, **whether or not anything is saved in Settings**: your
   edited system prompt from Settings' **Voice agent → Instructions** if you saved one, otherwise
   the shipped prompt; your additional instructions from there, if any; and always the send rules
   for the write token under the current **Sending** setting. So the agent follows that setting
   from its next call with no
   re-paste, and the prompt pasted here is no longer used at all. What it does not change is the
   `post_reply` approval from step 4: that lives at ElevenLabs, out of this server's reach, and is
   still changed by hand when the setting changes.

   Allow it only for an agent holding the **write token**. The page always gives it the write
   token's rules, because this server cannot see which token you gave the agent here; one holding
   the read token would be told it may send, and every send would be refused. If you later withdraw
   the permission, wait two minutes, or restart the server, before the next call: the server keeps
   the agent's answer that long, and an override the agent does not allow is believed to end the
   call.

   Without the override the agent keeps the prompt pasted here: your additional instructions reach
   it as a note at the start of each call, an edited system prompt does not reach it, and a change
   of the Sending setting needs the prompt pasted again. Settings says which happened on the last
   call, under Instructions and beside the Sending switch.

## Step 6 — Talk to it

Open the agent in ElevenLabs and start a conversation. Ask, roughly in this order:

1. *"What channels can you see?"* — exercises `list_channels`; proves the connection.
2. *"What's been happening in lead team?"* — exercises `digest_channel`; this is the real feature.
3. *"Read me the one about the mac runner."* — exercises `find_message`; this is the feature that
   makes it more than a text-to-speech bot.
4. *"Reply saying I'll look at it tonight."* — exercises `post_reply`. With "Ask me before the
   voice agent sends" on, **it must read the draft back as one fixed sentence — "To lead team:
   I'll look at it tonight. Send it?" — and nothing is posted until you confirm**. On the voice page
   a card appears with that text and a Send button, and on that page a plain "yes" as your very next
   answer to a read-back that was exactly that sentence also sends it; the agent says "One moment."
   and then tells you what the page reports. **In the ElevenLabs console there is no voice page in
   the call**, so no card, no spoken yes and no report: a "yes" there gets "One moment." and then
   silence, and the draft lapses after two minutes. That is expected; confirm from a call on the
   voice page. If a message appears in the channel before you confirm, stop: the server is not the
   one this guide describes. With the setting off, it posts at once and says so only after the
   server answered `SENT`. Switching the agent to the read token removes the capability rather than
   gating it.

Barge-in is native, so you can interrupt it mid-sentence without a button. That is not the
same as pausing: mute is what pauses, and it is this page's own doing — it withholds your audio,
which on the vendor's side looks exactly like you going quiet. The page therefore *tells* the agent
when you mute and when you unmute, which is what the paragraph you just pasted into the system
prompt is for. **Whether ElevenLabs honours that message is unverified** — it has never been run
against the live vendor — so if the agent still asks whether you are there during a long mute, that
is the open question and not a misconfiguration. Either way the call is still billed while muted:
the meter runs on the conversation being open, not on anyone speaking.

Conversation costs roughly $0.01/minute on ElevenLabs, and everything the read tools return —
your channel text — transits ElevenLabs and whatever model is behind it. That is inherent to
using a hosted voice agent, and it is worth knowing before you point it at a private channel.

---

## If something goes wrong

| Symptom | Almost always |
|---|---|
| Server exits immediately at startup | The two tokens are equal, or one is under 24 characters — or the startup channel probe refused. Read the last lines: the probe names each channel it could not read and what to do about it. |
| Startup says `Missing Access` for a channel | The bot is not in that Discord server, or it cannot view that channel. Re-do step 1.4/1.5, then step 1.6 for a private channel. |
| Startup says `Missing Permissions` for a channel | The bot can see the channel but lacks **Read Message History** there. Grant it in the channel's permission overrides — the invite is fine. |
| Startup says `Unknown Channel` (404) | The snowflake in `VIBE_TALK_CHANNELS` is wrong, or the bot was never invited to that server. |
| Startup says the token was rejected (401) | Wrong or regenerated bot token. Reset it in the Developer Portal. |
| Startup warns every message came back with EMPTY CONTENT | **Message Content Intent** (step 1.3). |
| `verify-deployment.sh` check 0 fails | Container is not running, or `--url` is wrong. `podman logs vibe-talk`. |
| Check 5 says `unknown_channel` | That snowflake is not in `VIBE_TALK_CHANNELS`. |
| Check 5 returns an **empty** digest | **Message Content Intent** (step 1.3). Second guess: the channel really is empty. |
| Check 5 mentions a Discord error | The bot is not in that channel, or lacks View Channel / Read Message History. |
| Check 6 says `channel_not_writable` | That channel is configured `ro`. Change it to `rw`. |
| Check 6 says direct sends are on | "Ask me before the voice agent sends" is off in Settings, so `post_reply` posted at once. Expected if you turned it off; otherwise tick it again. |
| Check 6 mentions a Discord error | The bot lacks Send Messages in that channel. |
| Public URL fails but localhost passed | The tunnel. Check `cloudflared` is running and the ingress hostname matches. |
| ElevenLabs cannot connect at all | Transport set to SSE instead of Streamable HTTP; or the URL is missing `/mcp`; or Cloudflare Access is in the way. |
| ElevenLabs connects but shows four tools | You gave it the read token. That is correct and deliberate — `post_reply` is hidden from a read credential. |
| The custom-MCP-server option is not there | Zero Retention Mode or HIPAA workspace. This path is closed on those. |

**If the server is up but something is wrong and you cannot tell what**, ask it:

```sh
curl -s -H "authorization: Bearer $VIBE_TALK_READ_TOKEN" \
  http://127.0.0.1:8080/api/v1/diagnostics
```

`GET /api/v1/diagnostics` re-runs the startup checks on demand and answers with a structured
report — the bot token, every configured channel, the ElevenLabs key, agent and voice, and the
store — where **every failing check carries a remedy** saying what to do about it. It always
answers HTTP 200, because the report is the answer, and it contains no credential. See
`README.md` → *Asking the server what is wrong*.

Deeper reference — the API, the security model and its honest limits, the known gaps — is in
`README.md`.
