# Can this bridge scope a summary to "since I last messaged"?

**Verdict: no, on both halves, and the second half is the one that bites.**

Research for `#61 unread-status`, checked 2026-08-19 against Discord's developer
documentation and against this crate's source. Claims that could not be confirmed from this
host are marked **unverified** and are never used to carry a conclusion, following the
convention `RELATED_WORK.md` already sets.

Note on links: `discord.com/developers/docs/*` now 301s to `docs.discord.com/developers/*`.
Every URL below is the post-redirect host, because the old ones rot.

## 1. Discord does not share read/unread state with a bot

Read state is a **client** concept. Verified by reading the pages, not by recollection:

* [`resources/message`](https://docs.discord.com/developers/resources/message) documents the
  full message endpoint set — Get Channel Messages, Get Channel Message, Search Guild
  Messages, Create Message, the reaction routes, the pin routes. **None of them is an ack or
  a read-state route**, and the strings "unread" and "read state" do not appear on the page
  at all.
* [`resources/channel`](https://docs.discord.com/developers/resources/channel) gives the
  whole channel object. The closest field is `last_message_id`, documented as "the id of the
  last message sent in this channel ... (may not point to an existing or valid message or
  thread)". That is **channel-wide metadata, not a per-user read position**: it says what the
  newest message is, not which messages any particular human has seen. There is no
  read-state field of any kind.
* [`events/gateway-events`](https://docs.discord.com/developers/events/gateway-events) gives
  the READY payload as exactly `v`, `user`, `guilds`, `session_id`, `resume_gateway_url`,
  `shard?`, `application`. There is no `read_state`, and neither the Send nor the Receive
  table contains `MESSAGE_ACK` or `CHANNEL_ACK`.
* [`topics/threads`](https://docs.discord.com/developers/topics/threads) says it outright:
  "Clients use *their own* thread member to calculate read states and notification settings.
  **This is largely irrelevant for bots though.**"

And this server has no gateway connection at all (see the module doc of `src/discord/mod.rs`:
"this server has no gateway connection and no webhook receiver"), so even a gateway-only
affordance would be out of reach without new machinery.

## 2. We cannot mark messages read either

There is no documented ack endpoint in the developer API. `POST
/channels/{id}/messages/{id}/ack` belongs to Discord's undocumented client/user API.

The ToS half is **unverified**: both
<https://support-dev.discord.com/hc/en-us/articles/8563934450327> and the Developer Terms of
Service article it redirects alongside returned **HTTP 403 to this host**, so the self-bot
prohibition could not be quoted first-hand. What would verify it: fetch those articles from a
host with unfiltered egress, and separately observe the actual status a real bot token gets
from that route.

## 3. Even if it existed, it would be the wrong subject

`authorization_header` (`src/discord/http.rs:68`) sends `Bot <token>`. The identity on every
request is the **bot application**, not the owner. A bot's read position is not the owner's
read position, and the owner reads Discord in his own client, where nothing in this system
observes him.

## 4. "Since the owner's own last message in this channel" is not computable today

This is the substantive finding, because it is the fallback the issue proposed. Five
blockers, each with a code anchor:

1. **The owner has no identity in this server.** `DiscordConfig` (`src/config.rs`) is only
   `{bot_token, api_base, default_fetch_limit, max_fetch_limit}`, and `ChannelInfo`
   (`src/model.rs`) is only `{id, label, writable}`. There is no `owner_id`, no
   `owner_user_id`, no `guild` anywhere. An agent would have to guess the owner from
   `DigestEntry.author` — a display name that `parse_message`
   (`src/discord/http.rs:157-160`) takes preferentially from `global_name`, which is
   attacker-settable. That makes a spoofable string the pivot of the decision about what the
   owner gets told.
2. **The digest cannot even separate humans from bots.** `Message` carries `author_is_bot`
   (`src/model.rs:103`), but `DigestEntry` (`src/summary.rs`) drops it.
3. **The owner's own replies are posted AS THE BOT.** `ops::reply` goes to
   `DiscordClient::post_message`, so "the last time I messaged them" is the bridge's own last
   post, not a message authored by the owner's human account. And the server never calls
   `GET /users/@me` (grep: `@me` appears nowhere), so it cannot even tell its own posts apart
   from the other coding agents' bot posts.
4. **The window truncates the question silently.** `fetch_request`
   (`src/discord/http.rs:74`) only ever emits `?limit=N`, clamped to `DISCORD_MAX_LIMIT` =
   100. `before` / `after` / `around` are documented and mutually exclusive, but this client
   never sends them and `DiscordClient` has no method that accepts them. When the anchor is
   older than the window, the digest simply does not contain it, and the agent cannot
   distinguish "he spoke three messages ago" from "he has not spoken here in a month".
5. **Reverse-chronological indexing is left to the model.** `run_digest`
   (`src/mcp/protocol.rs`) renders `[id | timestamp | author <@id>] summary` with no ordinal,
   so "fifteen messages ago" means counting list positions across fifty lines.

## 5. The constructive path, and why it is not a conclusion (**unverified**)

[`Search Guild Messages`](https://docs.discord.com/developers/resources/message) is now
documented in the developer API, and on paper it answers "when did user X last post in
channel C" without the 100-message ceiling: `author_id` (array, max 100), `channel_id`
(array, max 500), `sort_by` / `sort_order`, `min_id` / `max_id`, `limit` 1-25. Pair the
anchor it returns with `?after=<anchor>&limit=100` and you have exactly the messages since.

Caveats, quoted from that same page:

* "This endpoint is restricted according to whether the `MESSAGE_CONTENT` Privileged Intent
  is enabled for your application."
* "If the entity you are searching is not yet indexed, the endpoint will return a 202
  accepted response ... You should retry the request after the timeframe specified in the
  `retry_after` field."
* "when messages are actively being created or deleted, the `total_results` field may not be
  accurate."

It also needs a `guild.id`, which this deployment does not have: configuration carries
channel snowflakes only. `guild_id` is a documented channel-object field, so `GET
/channels/{channel.id}` at startup would supply it — also not implemented. And per README's
own Known gaps, the Discord layer has never run against live Discord, so none of this is
confirmed. It is a design sketch, not a capability.

## 6. Verdict for the wording of `#60 canned-prompt-buttons`

The prompt "Summarize my unread messages from the coding agent since I last messaged them"
claims a scoping the data cannot support, and must not ship as written.

Honest alternatives, in order of cost:

* **"Summarize the last N messages in \<channel\>."** This is what the code does today, and it
  is true today.
* **"Summarize what has happened since the bridge last posted here, and tell me if you can't
  see that far back."** Becomes truthful only after owner/self identity and a
  window-saturation signal exist.

## 7. Follow-on work this deliberately does not do

Named here so it can be scheduled without colliding with other clusters:

* add `author_is_bot` to `DigestEntry` (`src/summary.rs`, `src/mcp/protocol.rs`,
  `tests/api.rs`);
* add an `owner_user_id` configuration key (`src/config.rs`, `vibe-talk.example.toml`,
  `vibe-talk.env.example`);
* a `GET /users/@me` self-identity call (`src/discord/`);
* `after`-based pagination — the subject of `#53 stepped-retrieval`;
* a window-saturation signal in the digest header — the subject of
  `#62 message-count-accuracy`.

## What is guarded in code

`tests/mcp.rs::no_tool_description_claims_a_read_state_the_bot_cannot_have` reads every tool
description the live MCP endpoint hands a model and fails if any of them says "unread", "read
state", "marked read", "since you last", or "since I last". It also asserts that
`digest_channel` still says "most recent messages", so the guard cannot be satisfied by
emptying a description.

That guard covers the server-side manifest only. `#60`'s button prompt lives in the web page
and needs its own guard there.
