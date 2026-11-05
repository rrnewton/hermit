// Tests for the phone web app served at `/`, run against the REAL web/app.js and web/index.html.
//
// Why this exists at all: `/` and `/voice` are the two pages a person opens, they render the same
// facts about the same channels, and this repository has twice fixed one of them and shipped the
// other still wrong — `#52 operator-timezone` and `#62 message-count-accuracy` were both carried
// across afterwards, at the cost of a round trip each time. `#39 channel-alias` made it three:
// the alias reached /voice and the voice agent while this page went on printing the configured
// label, so one deployment showed two names for one channel.
//
// tests/js/voice_page.test.mjs has covered the other page for a long time. This file is the
// missing half, and it is built the same way and under the same rules:
//
//   * `getElementById` knows ONLY the ids that really appear in web/index.html, and THROWS for
//     anything else. A script reaching for an element the page does not have fails loudly here
//     instead of silently doing nothing on a phone.
//   * There is no innerHTML, no insertAdjacentHTML and no HTML parser anywhere in the fixture, so
//     a test cannot accidentally certify markup injection as "rendered". `page.createdTags`
//     records every element the page creates, which is how that is checked directly.
//   * A `<select>` moves to its first option when its options are replaced, exactly as a browser
//     does. That is not decoration: it is what makes "the picker is pointed at a channel" true
//     after a load, and a fixture without it would leave every read path unreachable.
//
// What this fixture CANNOT do: it lays nothing out, there is no speech engine, and there is no
// real network. It says what the page RENDERS, which is the whole of the claim being made here.
//
// No dependencies, no build step, no framework — same rule the page itself follows.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "web");
const SCRIPT = readFileSync(join(WEB, "app.js"), "utf8");
const HTML = readFileSync(join(WEB, "index.html"), "utf8");

/**
 * Every id web/index.html defines, with the tag and the class the MARKUP gives it.
 *
 * Taken from the file rather than restated here, so a page that stops shipping an element changes
 * the answer of the tests below instead of leaving them asserting against a fixture nobody serves.
 */
const PAGE_ELEMENTS = new Map(
  [...HTML.matchAll(/<([\w-]+)([^>]*\bid="([^"]+)"[^>]*)>([^<]*)/g)].map((m) => [
    m[3],
    {
      tag: m[1].toLowerCase(),
      className: (/\bclass="([^"]+)"/.exec(m[2]) || [])[1] || "",
      text: m[4].trim(),
    },
  ])
);

/** The tab buttons, which carry no id — they are found by `nav button` and read by `data-tab`. */
const NAV_BUTTONS = [...HTML.matchAll(/<button\b([^>]*\bdata-tab="([^"]+)"[^>]*)>/g)].map((m) => ({
  tab: m[2],
  className: (/\bclass="([^"]+)"/.exec(m[1]) || [])[1] || "",
}));

/** A JSON response, shaped exactly as the page's `api()` unpacks one. */
const json = (status, body) => ({
  ok: status < 400,
  status,
  text: async () => JSON.stringify(body),
});

/**
 * The channel this fixture serves, exactly as `ChannelInfo` serializes one.
 *
 * `alias` is deliberately absent rather than null, because that is what a server too old to have
 * `#39 channel-alias` sends, and the page has to survive it.
 */
const CHANNEL = { id: "1110000000000000001", label: "lead team", writable: true };

/** The same channel wearing a name of the owner's own. */
const ALIASED = { ...CHANNEL, alias: "the build channel" };

class FakeElement {
  constructor(id, tagName = "", className = "") {
    this.id = id;
    this.tagName = tagName;
    this.className = className;
    this.textContent = "";
    this.value = "";
    this.children = [];
    this.parentNode = null;
    this.dataset = {};
    this.listeners = new Map();
    this.classToggles = [];
  }

  addEventListener(type, fn) {
    const held = this.listeners.get(type) || [];
    held.push(fn);
    this.listeners.set(type, held);
  }

  async dispatch(type, event = {}) {
    for (const fn of this.listeners.get(type) || []) {
      await fn({ preventDefault() {}, ...event });
    }
  }

  click() {
    return this.dispatch("click");
  }

  /**
   * A `<select>` lands on its first option when its options are replaced, as a browser does.
   *
   * The page relies on it — nothing in web/app.js ever assigns a channel to a picker — so a
   * fixture that left the value empty would make every read path return early and every test
   * below assert about a page that never fetched anything.
   */
  settleValue() {
    if (this.tagName !== "select") {
      return;
    }
    const values = this.children.map((option) => option.value);
    if (!values.includes(this.value)) {
      this.value = values.length > 0 ? values[0] : "";
    }
  }

  append(...kids) {
    for (const kid of kids) {
      kid.parentNode = this;
    }
    this.children.push(...kids);
    this.settleValue();
  }

  replaceChildren(...kids) {
    for (const old of this.children) {
      old.parentNode = null;
    }
    for (const kid of kids) {
      kid.parentNode = this;
    }
    this.children = [...kids];
    this.settleValue();
  }

  setAttribute() {}

  classList = {
    toggle: (name, on) => {
      this.classToggles.push(`${name}=${on}`);
      const held = new Set(this.className.split(/\s+/).filter(Boolean));
      if (on) {
        held.add(name);
      } else {
        held.delete(name);
      }
      this.className = [...held].join(" ");
    },
  };

  scrollIntoView() {}

  /** Every element at or under this one, so a test can ask what was really rendered. */
  descendants() {
    const out = [];
    const walk = (node) => {
      out.push(node);
      for (const kid of node.children) {
        walk(kid);
      }
    };
    for (const kid of this.children) {
      walk(kid);
    }
    return out;
  }

  /** The visible text of this subtree, in document order. */
  text() {
    return [this, ...this.descendants()].map((node) => node.textContent).join(" ");
  }
}

/**
 * Build a page: the fake DOM, the fake server, and one real execution of web/app.js.
 *
 * The channels are an argument rather than something a test sets afterwards, and that is not
 * style: web/app.js reads `/api/v1/client-config` on the last line of the file, so the answer is
 * composed before `newPage` has returned. A test that assigned `page.channels` after the fact
 * would silently be describing the load it did not get.
 *
 * `store` is the browser's localStorage. Passing an existing one simulates a reload: same storage,
 * brand new script execution.
 */
function newPage(
  channels = [{ ...CHANNEL }],
  store = new Map([["vibe-talk.token", "read-token-aaaaaaaaaaaaaaaa"]]),
  chatProviderName = undefined,
  readAloud = undefined
) {
  const elements = new Map();
  for (const [id, markup] of PAGE_ELEMENTS) {
    elements.set(id, new FakeElement(id, markup.tag, markup.className));
    elements.get(id).textContent = markup.text;
  }
  const navButtons = NAV_BUTTONS.map((spec) => {
    const button = new FakeElement("", "button", spec.className);
    button.dataset.tab = spec.tab;
    return button;
  });

  const page = {
    elements,
    el: (id) => {
      const found = elements.get(id);
      if (!found) {
        throw new Error(`app.js asked for #${id}, which web/index.html does not define`);
      }
      return found;
    },
    navButtons,
    /** Let the page's own floating promise — the load-time config read — run to completion. */
    settle: () => new Promise((resolve) => setTimeout(resolve, 0)),
    /**
     * The channels this server is configured for, as `ops::channels` serializes them: the
     * configured label, plus the operator's own name for the channel when he has set one.
     */
    channels,
    /** The digest entries and the scrollback messages the fake server holds. */
    entries: [
      {
        id: "9000000000000000001",
        author: "alice",
        timestamp: "2026-08-19T20:40:00.000Z",
        spoken_time: "13:40",
        summary: "the arm64 job never reported",
      },
    ],
    messages: [
      {
        id: "9000000000000000001",
        channel_id: CHANNEL.id,
        author: "alice",
        author_is_bot: false,
        timestamp: "2026-08-19T20:40:00.000Z",
        spoken_time: "13:40",
        content: "the arm64 job never reported",
      },
    ],
    /** Every tag name the page has passed to createElement, in order. */
    createdTags: [],
    /** Everything the page has asked this browser to say out loud. */
    spoken: [],
    utterances: [],
    deviceVoices: [],
  };

  /** The channel the fake server answers WITH: the same overlay `ops::allowed` applies. */
  const served = (id) =>
    page.channels.find((channel) => String(channel.id) === String(id)) || page.channels[0];

  const document = {
    getElementById: (id) => elements.get(id) || null,
    createElement: (tag) => {
      page.createdTags.push(String(tag).toLowerCase());
      return new FakeElement("", String(tag).toLowerCase());
    },
    querySelectorAll: (selector) => {
      if (selector === "nav button") {
        return navButtons;
      }
      if (selector === ".tab") {
        return [...elements.values()].filter((element) =>
          element.className.split(/\s+/).includes("tab")
        );
      }
      throw new Error(`app.js selected ${JSON.stringify(selector)}, which this fixture does not model`);
    },
  };

  const localStorage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, v),
    removeItem: (k) => store.delete(k),
  };
  page.storage = store;

  const context = {
    document,
    localStorage,
    console,
    navigator: { language: "en-US" },
    setTimeout,
    window: {
      speechSynthesis: {
        cancel: () => {},
        getVoices: () => page.deviceVoices,
        speak: (utterance) => { page.spoken.push(utterance.text); page.utterances.push(utterance); },
      },
    },
    SpeechSynthesisUtterance: class {
      constructor(text) {
        this.text = text;
      }
    },
    fetch: async (path, options) => {
      const url = String(path);
      if (url.startsWith("/api/v1/client-config")) {
        return json(200, {
          version: "test",
          chat_provider_name: chatProviderName,
          channels: page.channels,
          elevenlabs_agent_id: null,
          read_aloud: readAloud,
          live_poll_seconds: 30,
        });
      }
      const digest = /^\/api\/v1\/channels\/([^/]+)\/digest$/.exec(url);
      if (digest) {
        return json(200, {
          channel: served(decodeURIComponent(digest[1])),
          entries: page.entries,
          complete: true,
        });
      }
      const one = /^\/api\/v1\/channels\/([^/]+)\/messages\/([^/]+)$/.exec(url);
      if (one) {
        return json(200, { message: page.messages[0] });
      }
      const scrollback = /^\/api\/v1\/channels\/([^/]+)\/messages$/.exec(url);
      if (scrollback) {
        return json(200, {
          channel: served(decodeURIComponent(scrollback[1])),
          messages: page.messages,
          complete: true,
        });
      }
      const resolve = /^\/api\/v1\/channels\/([^/]+)\/resolve$/.exec(url);
      if (resolve) {
        return json(200, {
          best: { message: page.messages[0], matched_terms: ["arm64"] },
          ambiguous: false,
          searched: page.messages.length,
        });
      }
      throw new Error(`the page fetched ${url}, which this fixture does not serve`);
    },
  };
  vm.createContext(context);
  vm.runInContext(SCRIPT, context, { filename: "app.js" });
  return page;
}

/** A page that has finished its load-time read of `/api/v1/client-config`. */
async function loaded(channels) {
  const page = newPage(channels);
  await page.settle();
  return page;
}

/** The text of each option in a picker, in order. */
const optionText = (page, id) => [...page.el(id).children].map((option) => option.textContent);

/** Both pickers on this page. Named once: the whole finding was a fix applied to some of them. */
const PICKERS = ["talk-channel", "text-channel"];

// --- the elements the script and the markup have to agree about ------------------------------

test("the page and the script agree about which elements exist", async () => {
  // The same control the /voice suite carries. `getElementById` throws for an unknown id, so this
  // is really an assertion that a full load touched nothing web/index.html does not ship.
  const page = await loaded();
  await page.el("refresh-digest").click();
  await page.el("refresh-text").click();
  await page.settle();
  assert.equal(page.el("status").textContent, "1 message");
});

test("server information names the selected chat backend, with a neutral fallback", async () => {
  for (const name of ["Google Chat", "Discord", undefined, null, "   ", "<img src=x>"]) {
    const page = newPage(undefined, undefined, name);
    await page.settle();
    const expected = typeof name === "string" ? name.trim() || "Chat" : "Chat";
    assert.ok(page.el("server-info").textContent.includes(`chat service ${expected};`));
    assert.ok(!page.createdTags.includes("img"), "a backend name was interpreted as markup");
  }
});

// --- `#39 channel-alias`: one channel, one name ------------------------------------------------
//
// The alias is the OWNER's own name for a channel, stored locally by this server and never sent to
// Discord. `ChannelInfo::display_name` is the rule; `channelName` in web/app.js is the same rule
// in the browser. This page renders a channel's name in three places — both pickers and the
// header — and the claim being tested is that all three obey it and fall back the same way.

test("BOTH CHANNEL PICKERS SHOW THE NAME THE OWNER GAVE THE CHANNEL", async () => {
  // The control first, in the same test, because "the picker shows a name" is satisfied by a page
  // that only ever shows the configured label.
  const plain = await loaded();
  for (const id of PICKERS) {
    assert.deepStrictEqual(
      optionText(plain, id),
      ["lead team (postable)"],
      `with no alias set, #${id} shows the configured label`
    );
  }

  const page = await loaded([{ ...ALIASED }]);
  for (const id of PICKERS) {
    assert.deepStrictEqual(
      optionText(page, id),
      ["the build channel (postable)"],
      `#${id} must show the owner's own name for the channel`
    );
    assert.deepStrictEqual(
      [...page.el(id).children].map((option) => option.value),
      [CHANNEL.id],
      "the VALUE is still the snowflake: it is what every route on the server takes"
    );
  }
});

test("the postable suffix survives the rename, and a read-only channel still gets none", async () => {
  const page = await loaded([
    { ...ALIASED },
    { id: "1110000000000000002", label: "build noise", writable: false, alias: "the noisy one" },
  ]);
  for (const id of PICKERS) {
    assert.deepStrictEqual(optionText(page, id), [
      "the build channel (postable)",
      "the noisy one",
    ]);
  }
});

test("THE CHANNEL-NAME HEADER USES THE ALIAS ON THE DIGEST PATH", async () => {
  const plain = await loaded();
  await plain.el("refresh-digest").click();
  await plain.settle();
  assert.equal(
    plain.el("channel-name").textContent,
    "lead team",
    "the control: with no alias the header shows the configured label"
  );

  const page = await loaded([{ ...ALIASED }]);
  await page.el("refresh-digest").click();
  await page.settle();
  assert.equal(page.el("channel-name").textContent, "the build channel");
});

test("...AND ON THE SCROLLBACK PATH, WHICH IS THE ONE THAT GETS FORGOTTEN", async () => {
  // Two writers of one header, and the second one is where `#62 message-count-accuracy` was
  // missed the first time. Arriving on the Text tab first is an ordinary way to use this page.
  const plain = await loaded();
  await plain.el("refresh-text").click();
  await plain.settle();
  assert.equal(plain.el("channel-name").textContent, "lead team", "the control");

  const page = await loaded([{ ...ALIASED }]);
  await page.el("refresh-text").click();
  await page.settle();
  assert.equal(page.el("channel-name").textContent, "the build channel");
});

test("changing the picker re-reads THAT channel and renames the header with it", async () => {
  const page = await loaded([
    { ...CHANNEL },
    { id: "1110000000000000002", label: "build noise", writable: false, alias: "the noisy one" },
  ]);
  page.el("text-channel").value = "1110000000000000002";
  await page.el("text-channel").dispatch("change");
  await page.settle();
  assert.equal(page.el("channel-name").textContent, "the noisy one");
});

test("A CHANNEL WITH NO ALIAS AT ALL FALLS BACK TO THE CONFIGURED LABEL", async () => {
  // Three ways a server can decline to name a channel, all of which must read the same: a field
  // that is absent because the server predates the feature, an explicit null because nothing is
  // stored, and — the one a bare `alias || label` gets wrong in the other direction — a value
  // that is only whitespace. The server refuses a blank one, so this is a defence against a
  // store that already holds one, not against a route that would accept it.
  for (const alias of [undefined, null, "", "   "]) {
    const page = await loaded([{ ...CHANNEL, alias }]);
    await page.el("refresh-digest").click();
    await page.settle();
    assert.deepStrictEqual(
      optionText(page, "talk-channel"),
      ["lead team (postable)"],
      `an alias of ${JSON.stringify(alias)} must leave the configured label in the picker`
    );
    assert.equal(
      page.el("channel-name").textContent,
      "lead team",
      `an alias of ${JSON.stringify(alias)} must leave the configured label in the header`
    );
  }
});

test("clearing the alias on the server puts the configured label back on the next read", async () => {
  // The page has no editor — the alias is set from /voice Settings, which is the operator's, and
  // no MCP tool can reach it at all. What this page has to get right is that it shows whatever the
  // server currently says, rather than caching the first name it was given.
  const page = await loaded([{ ...ALIASED }]);
  await page.el("refresh-text").click();
  await page.settle();
  assert.equal(page.el("channel-name").textContent, "the build channel", "the control");

  page.channels = [{ ...CHANNEL, alias: null }];
  await page.el("refresh-text").click();
  await page.settle();
  assert.equal(page.el("channel-name").textContent, "lead team");
});

test("an alias is text the page renders as CHARACTERS, never as markup", async () => {
  // Rule 1 at the head of web/app.js. An alias is typed by the operator rather than by a stranger,
  // but it arrives over the same wire as everything else and it lands in the same two sinks.
  const page = await loaded([{ ...CHANNEL, alias: "<script>alert(1)</script>" }]);
  await page.el("refresh-digest").click();
  await page.settle();
  assert.equal(
    page.el("channel-name").textContent,
    "<script>alert(1)</script>",
    "the header holds the characters, so the alias is visible for what it is"
  );
  assert.deepStrictEqual(
    optionText(page, "talk-channel"),
    ["<script>alert(1)</script> (postable)"],
    "and so does the picker"
  );
  // The whole set the page built, named literally rather than filtered: a filter written around
  // today's tags would let tomorrow's <script> through by not mentioning it.
  assert.deepStrictEqual(
    [...new Set(page.createdTags)].sort(),
    ["div", "li", "option", "p", "span"],
    "the page created an element it does not create for an ordinary channel name"
  );
});


test("the digest page selects a local device voice only when that backend is configured", async () => {
  const remote = { name: "Remote", lang: "en-US", localService: false, default: true };
  const first = { name: "First installed", lang: "en-US", localService: true, default: false };
  const local = { name: "Configured", lang: "en-US", localService: true, default: true };
  const wrong = { name: "Wrong default", lang: "fr-FR", localService: true, default: true };
  const page = newPage(undefined, undefined, undefined, { playback: "browser", local_only: true });
  page.deviceVoices = [remote, wrong];
  await page.settle();
  await page.el("refresh-digest").click();
  await page.el("speak-digest").click();
  assert.equal(page.spoken.length, 0);
  assert.match(page.el("status").textContent, /matches this browser's language/);
  page.deviceVoices = [remote, wrong, first, local];
  await page.el("speak-digest").click();
  assert.equal(page.utterances.length, 1);
  assert.equal(page.utterances[0].voice, local);
  assert.equal(page.utterances[0].lang, "en-US");
  page.utterances[0].onerror();
  assert.match(page.el("status").textContent, /could not play/);
});

test("the digest page preserves browser-default playback for an older server", async () => {
  const page = await loaded();
  await page.el("refresh-digest").click();
  await page.el("speak-digest").click();
  assert.equal(page.spoken.length, 1);
  assert.equal(page.utterances[0].voice, undefined);
});

test("replacing or forgetting the shared token takes the voice page's saved messages with it", async () => {
  // `#18 offline-message-cache`: the voice page keeps channel rows on the device for the token that
  // read them, and this page can change that token without the voice page ever running.
  const CACHE = "vibe-talk.voice.message-cache";
  const store = new Map([["vibe-talk.token", "read-token-aaaaaaaaaaaaaaaa"], [CACHE, "{\"v\":1}"]]);
  const page = newPage(undefined, store);
  await page.settle();
  page.el("api-token").value = "read-token-aaaaaaaaaaaaaaaa";
  await page.el("save-token").click();
  assert.equal(store.has(CACHE), true, "saving the SAME token discarded rows it can still read");
  page.el("api-token").value = "read-token-bbbbbbbbbbbbbbbb";
  await page.el("save-token").click();
  assert.equal(store.has(CACHE), false, "another token inherited the saved messages");

  store.set(CACHE, "{\"v\":1}");
  await page.el("forget-token").click();
  assert.equal(store.has(CACHE), false, "signing out left the saved messages on the device");
});
