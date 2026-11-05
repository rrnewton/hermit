"use strict";
// vibe-talk phone client.
//
// Two rules hold everywhere in this file:
//
// 1. Channel text is UNTRUSTED. It is inserted with textContent, never innerHTML, so a message
//    can never become markup, script, or a link the owner did not intend to tap. There is no
//    markdown rendering here on purpose.
// 2. The API token lives only in this browser's localStorage and is sent as a bearer token. It is
//    never put in a URL, so it cannot leak through a referrer or a server log.

const TOKEN_KEY = "vibe-talk.token";
// The voice page's saved channel rows, readable only with the token that read them. This page
// shares that token, so replacing or forgetting it here must take them too.
const MESSAGE_CACHE_KEY = "vibe-talk.voice.message-cache";
// Read, never written. See the matching comment in voice.js: a browser that signed in before the
// service was renamed still holds its token under the old key on this same origin, and saying so
// is the difference between "you were signed out" and "this deployment is broken".
const RENAMED_FROM_KEY = "gent-talk.token";
const state = {
  channels: [],
  digest: [],
  agentId: null,
  readAloud: null,
};

const el = (id) => document.getElementById(id);
const setStatus = (text) => {
  el("status").textContent = text;
};

function token() {
  return localStorage.getItem(TOKEN_KEY) || "";
}

async function api(path, options = {}) {
  const headers = Object.assign({ Authorization: `Bearer ${token()}` }, options.headers || {});
  if (options.body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(path, Object.assign({}, options, { headers }));
  const text = await response.text();
  let payload = null;
  try {
    payload = text ? JSON.parse(text) : null;
  } catch (_error) {
    throw new Error(`server returned non-JSON (HTTP ${response.status})`);
  }
  if (!response.ok) {
    const detail = payload && payload.detail ? payload.detail : `HTTP ${response.status}`;
    // 403 is the right token with the wrong scope, and this page cannot tell which one was pasted
    // until something writes: /api/v1/client-config answers a read-scope token exactly as it
    // answers a write-scope one, so a read token loads the whole interface and then fails on the
    // first Reply. The server's sentence alone ("this token may read but not post") leaves the
    // reader to work out that there IS a second token and which config key holds it.
    const error = new Error(
      response.status === 403
        ? `${detail} — that looks like a READ token; replying needs the write-scope one ` +
          `(VIBE_TALK_WRITE_TOKEN rather than VIBE_TALK_READ_TOKEN)`
        : detail
    );
    error.status = response.status;
    throw error;
  }
  return payload;
}

/**
 * True when nothing is stored under the current key but something is stored under the pre-rename
 * one. Guarded, because a browser with storage disabled throws on `getItem` rather than answering,
 * and this runs on the load path.
 */
function signedOutByRename() {
  try {
    return !localStorage.getItem(TOKEN_KEY) && !!localStorage.getItem(RENAMED_FROM_KEY);
  } catch (_error) {
    return false;
  }
}

// How many messages there are, or an honest refusal to say.
//
// The length of what the server returned is the FETCH WINDOW, not a channel total. Discord gives
// a bot no message count for a guild text channel, so the number is the channel's own only when
// the server reports `complete` -- the fetch came back short, meaning there is nothing older.
// Otherwise no digit is shown at all: a confidently wrong count is worse than no count, and this
// one was wrong in the direction that makes the bridge look like it is losing messages.
//
// `!== true` rather than `=== false`, so a server too old to send the field is treated as unknown.
function messageCount(count, complete) {
  if (complete !== true) {
    return "the most recent messages — older ones are not loaded";
  }
  if (count === 0) {
    return "no messages";
  }
  return `${count} message${count === 1 ? "" : "s"}`;
}

function messageNode(message, opts = {}) {
  const li = document.createElement("li");
  const meta = document.createElement("div");
  meta.className = "meta";

  const author = document.createElement("span");
  author.textContent = message.author;
  const stamp = document.createElement("span");
  // `#52 operator-timezone`. The server converts once, into the operator's configured zone, and
  // hands back a string that is already correct — so prefer it. The ISO slice below is the
  // fallback for a server too old to send `spoken_time`, and it is UTC-as-Discord-reported-it,
  // which is exactly the value the phone and the voice agent must not disagree about.
  stamp.textContent =
    message.spoken_time || (message.timestamp || "").replace("T", " ").slice(0, 16);
  meta.append(author, stamp);

  const body = document.createElement("div");
  body.className = "body";
  body.textContent = opts.summary !== undefined ? opts.summary : message.content;

  li.append(meta, body);
  if (opts.onSelect) {
    li.addEventListener("click", () => opts.onSelect(message));
  }
  return li;
}

// What to call a channel. `#39 channel-alias`.
//
// The alias when the owner has given the channel one, the configured label otherwise. It is the
// server's `ChannelInfo::display_name` rule, and it is stated here because THIS page renders a
// channel's name in three places -- both pickers and the header -- and a rule applied to one of
// them is how a deployment comes to show "build noise" here and "the build channel" on /voice.
//
// The same function, under the same name, exists in web/voice.js. Two served assets with no build
// step between them cannot share a module, so what keeps them from drifting is a test over the
// bytes of BOTH files in src/http/api.rs -- the same guard that had to be widened after `#52
// operator-timezone` and `#62 message-count-accuracy` were each fixed on one page and not the
// other.
//
// Trimmed and emptiness-checked rather than a bare `alias || label`: the server refuses a blank
// alias, so an empty string arriving here means something upstream is wrong, and a picker with a
// blank entry in it is worse than one showing the configured label.
//
// Nothing on this page SETS an alias. That is the operator's control in /voice Settings, and the
// agent has no tool for it at all; this page is a reader of the name, like everything else.
function channelName(channel) {
  if (!channel) {
    return "this channel";
  }
  const alias = typeof channel.alias === "string" ? channel.alias.trim() : "";
  return alias || String(channel.label || channel.id || "this channel");
}

function fillChannelSelects() {
  for (const id of ["talk-channel", "text-channel"]) {
    const select = el(id);
    const previous = select.value;
    select.replaceChildren();
    for (const channel of state.channels) {
      const option = document.createElement("option");
      option.value = channel.id;
      const name = channelName(channel);
      option.textContent = channel.writable ? `${name} (postable)` : name;
      select.append(option);
    }
    if (previous) {
      select.value = previous;
    }
  }
}

async function loadConfig() {
  if (!token()) {
    setStatus(
      signedOutByRename()
        ? "this deployment was renamed, so your saved token needs pasting again — open Settings"
        : "no API token yet — open Settings"
    );
    return;
  }
  const config = await api("/api/v1/client-config");
  state.channels = config.channels;
  state.agentId = config.elevenlabs_agent_id;
  state.readAloud = config.read_aloud || null;
  if (state.readAloud && state.readAloud.playback === "browser") {
    // Voice discovery may be asynchronous; begin before the reader presses Read.
    window.speechSynthesis?.getVoices?.();
  }
  fillChannelSelects();
  const providerName =
    typeof config.chat_provider_name === "string" ? config.chat_provider_name.trim() : "";
  el("server-info").textContent =
    `server version ${config.version}; ` +
    `chat service ${providerName || "Chat"}; ` +
    `${config.channels.length} channel${config.channels.length === 1 ? "" : "s"}; ` +
    `voice agent ${config.elevenlabs_agent_id ? "configured" : "not configured"}`;
  mountVoiceAgent();
  setStatus("connected");
}

// The seam. When an ElevenLabs agent id is configured, mount the vendor's hosted widget; the agent
// itself reaches this server's API over the network, using its own credential. When it is not
// configured, say so plainly rather than pretending there is a voice path.
function mountVoiceAgent() {
  const host = el("voice-agent");
  host.replaceChildren();
  if (!state.agentId) {
    const note = document.createElement("p");
    note.className = "muted";
    note.textContent =
      "Voice agent not configured. Set elevenlabs.agent_id on the server to mount the hosted " +
      "widget here. Until then, 'Read the digest aloud' uses this phone's own speech engine.";
    host.append(note);
    return;
  }
  const widget = document.createElement("elevenlabs-convai");
  widget.setAttribute("agent-id", state.agentId);
  const script = document.createElement("script");
  script.src = "https://unpkg.com/@elevenlabs/convai-widget-embed";
  script.async = true;
  script.type = "text/javascript";
  host.append(widget, script);
}

async function loadDigest() {
  const channel = el("talk-channel").value;
  if (!channel) {
    return;
  }
  setStatus("fetching…");
  const payload = await api(`/api/v1/channels/${encodeURIComponent(channel)}/digest`);
  state.digest = payload.entries;
  const list = el("digest");
  list.replaceChildren();
  for (const entry of payload.entries) {
    list.append(
      messageNode(
        {
          author: entry.author,
          timestamp: entry.timestamp,
          spoken_time: entry.spoken_time,
          content: entry.summary,
        },
        { onSelect: () => readFull(channel, entry.id) }
      )
    );
  }
  el("channel-name").textContent = channelName(payload.channel);
  setStatus(messageCount(payload.entries.length, payload.complete));
}

async function readFull(channel, messageId) {
  const payload = await api(
    `/api/v1/channels/${encodeURIComponent(channel)}/messages/${encodeURIComponent(messageId)}`
  );
  showResult(payload.message, null);
  speak(`${payload.message.author} said: ${payload.message.content}`);
}

function showResult(message, note) {
  const host = el("find-result");
  host.replaceChildren();
  if (note) {
    const warn = document.createElement("p");
    warn.className = "ambiguous";
    warn.textContent = note;
    host.append(warn);
  }
  if (message) {
    const list = document.createElement("ol");
    list.className = "messages";
    list.append(messageNode(message));
    host.append(list);
  }
}

async function findMessage(event) {
  event.preventDefault();
  const channel = el("talk-channel").value;
  const query = el("find-query").value.trim();
  if (!channel || !query) {
    return;
  }
  setStatus("searching…");
  const payload = await api(`/api/v1/channels/${encodeURIComponent(channel)}/resolve`, {
    method: "POST",
    body: JSON.stringify({ query }),
  });
  if (!payload.best) {
    showResult(null, `nothing in the last ${payload.searched} messages matched that.`);
    setStatus("no match");
    speak("Nothing in the recent messages matched that.");
    return;
  }
  const note = payload.ambiguous
    ? "More than one message fits that description; this is the closest."
    : null;
  showResult(payload.best.message, note);
  speak(`${payload.best.message.author} said: ${payload.best.message.content}`);
  setStatus(`matched on: ${payload.best.matched_terms.join(", ")}`);
}

async function loadScrollback() {
  const channel = el("text-channel").value;
  if (!channel) {
    return;
  }
  setStatus("fetching…");
  const payload = await api(`/api/v1/channels/${encodeURIComponent(channel)}/messages`);
  const list = el("scrollback");
  list.replaceChildren();
  for (const message of payload.messages) {
    list.append(messageNode(message));
  }
  // The header names the channel you are looking at. Only the digest path used to set it, so
  // arriving on the Text tab first left the header reading "not connected" over a full,
  // freshly-fetched scrollback -- the one place the app lied about its own state.
  el("channel-name").textContent = channelName(payload.channel);
  setStatus(messageCount(payload.messages.length, payload.complete));
  list.lastElementChild?.scrollIntoView({ block: "end" });
}

// The browser's speech service reads the digest. A configured device backend selects only a
// voice reported as local; the phone's speech-engine settings determine how audio is generated.
function speak(text) {
  if (!("speechSynthesis" in window) || typeof SpeechSynthesisUtterance !== "function") {
    setStatus("This browser has no speech engine. Open this page in Chrome on Android.");
    return;
  }
  const utterance = new SpeechSynthesisUtterance(text);
  if (state.readAloud && state.readAloud.playback === "browser") {
    const voices = (window.speechSynthesis.getVoices?.() || [])
      .filter((voice) => voice.localService === true);
    const languages = [
      ...(Array.isArray(navigator.languages) ? navigator.languages : []),
      navigator.language,
    ].map((language) => String(language || "").replace(/_/gu, "-").toLowerCase())
      .filter((language, index, all) => language && all.indexOf(language) === index);
    const tagged = voices.map((voice) => ({
      voice,
      language: String(voice.lang || "").replace(/_/gu, "-").toLowerCase(),
    }));
    let voice = null;
    for (const language of languages) {
      const exact = tagged.filter((entry) => entry.language === language);
      voice = (exact.find((entry) => entry.voice.default) || exact[0])?.voice || null;
      if (voice) break;
    }
    for (const language of languages) {
      if (voice) break;
      const base = language.split("-")[0];
      const compatible = tagged.filter((entry) => entry.language.split("-")[0] === base);
      voice = (compatible.find((entry) => entry.voice.default) || compatible[0])?.voice || null;
    }
    if (!voice) {
      setStatus("No installed device voice matches this browser's language. Download one in Android Text-to-speech settings, then tap Read again.");
      return;
    }
    utterance.voice = voice;
    utterance.lang = voice.lang;
    utterance.onerror = () => setStatus("Device speech could not play. Check your media volume and installed voices, then tap Read again.");
  }
  window.speechSynthesis.cancel();
  window.speechSynthesis.speak(utterance);
}

function speakDigest() {
  if (state.digest.length === 0) {
    speak("Nothing fetched yet.");
    return;
  }
  const lines = state.digest.map((entry) => `${entry.author}: ${entry.summary}`);
  speak(`${state.digest.length} recent messages. ${lines.join(". ")}`);
}

function guard(fn) {
  return (...args) =>
    Promise.resolve(fn(...args)).catch((error) => setStatus(`error: ${error.message}`));
}

function wire() {
  for (const button of document.querySelectorAll("nav button")) {
    button.addEventListener("click", () => {
      for (const other of document.querySelectorAll("nav button")) {
        other.classList.toggle("active", other === button);
      }
      for (const section of document.querySelectorAll(".tab")) {
        section.classList.toggle("active", section.id === `tab-${button.dataset.tab}`);
      }
    });
  }

  el("refresh-digest").addEventListener("click", guard(loadDigest));
  el("refresh-text").addEventListener("click", guard(loadScrollback));
  el("find-form").addEventListener("submit", guard(findMessage));
  el("speak-digest").addEventListener("click", speakDigest);
  el("stop-speaking").addEventListener("click", () => window.speechSynthesis?.cancel());
  el("talk-channel").addEventListener("change", guard(loadDigest));
  el("text-channel").addEventListener("change", guard(loadScrollback));

  el("api-token").value = token();
  el("save-token").addEventListener(
    "click",
    guard(async () => {
      const next = el("api-token").value.trim();
      if (next !== token()) localStorage.removeItem(MESSAGE_CACHE_KEY);
      localStorage.setItem(TOKEN_KEY, next);
      await loadConfig();
    })
  );
  el("forget-token").addEventListener("click", () => {
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(MESSAGE_CACHE_KEY);
    el("api-token").value = "";
    setStatus("token forgotten");
  });
}

wire();
guard(loadConfig)();
