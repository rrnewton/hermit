// The browser's generated validators accept what the server sends and refuse what it cannot.
//
// `contract/samples.json` is serialized by the real Rust types (`tests/contract.rs` fails when it is
// stale), so every sample passing here is the claim "the page accepts every answer this server can
// give": every union variant, and every optional field both present and omitted. The refusals
// below are the other half. A validator that accepted everything would pass the first half alone.
//
// `web/contract.js` is loaded exactly as the page loads it: as a classic script into a fresh global,
// with no module loader and no Ajv package present. That is what proves the generated file is
// standalone, which is what lets it be served with no build step.
//
// Run with `node tests/js/contract.test.mjs`; `cargo test --test contract` runs it too.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const read = (path) => readFileSync(join(ROOT, path), "utf8");

const SCHEMA = JSON.parse(read("contract/vibe-talk.schema.json"));
const SAMPLES = JSON.parse(read("contract/samples.json"));
const DECLARATIONS = read("web/contract.d.ts");

function loadContract() {
  const context = vm.createContext({});
  vm.runInContext(read("web/contract.js"), context, { filename: "contract.js" });
  return context.VibeTalkContract;
}

const contract = loadContract();
const definition = (name) => SCHEMA.$defs[name];
const clone = (value) => JSON.parse(JSON.stringify(value));

test("the browser knows exactly the roots the schema names", () => {
  const roots = SCHEMA.anyOf.map((root) => root.$ref.replace("#/$defs/", ""));
  assert.deepEqual([...contract.roots], roots);
  assert.deepEqual(Object.keys(SAMPLES).sort(), [...roots].sort());
  assert.ok(Object.isFrozen(contract), "a page script could replace a validator");
});

test("every sample the Rust types serialize is accepted", () => {
  for (const [root, samples] of Object.entries(SAMPLES)) {
    assert.ok(samples.length > 0, `${root} has no samples`);
    for (const sample of samples) {
      assert.equal(contract.is(root, sample), true, `${root} refused ${JSON.stringify(sample)}`);
      assert.deepEqual(contract.decode(root, clone(sample)), sample);
    }
  }
});

test("removing any required field of a sample is refused, naming the field", () => {
  let refusals = 0;
  for (const [root, samples] of Object.entries(SAMPLES)) {
    const required = definition(root).required || [];
    for (const sample of samples) {
      for (const field of required) {
        const broken = clone(sample);
        delete broken[field];
        assert.equal(contract.is(root, broken), false, `${root} accepted a value without ${field}`);
        assert.throws(() => contract.decode(root, broken), (error) => {
          assert.equal(error.contract, root);
          assert.match(error.message, new RegExp(`^malformed ${root}: .*'${field}'`));
          return true;
        });
        refusals += 1;
      }
    }
  }
  assert.ok(refusals > 100, `only ${refusals} refusals were exercised`);
});

test("a wrong type, a wrong enum spelling, and a non-object are refused", () => {
  const config = clone(SAMPLES.ClientConfigResponse[0]);
  assert.equal(contract.is("ClientConfigResponse", { ...config, live_poll_seconds: "5" }), false);
  assert.equal(contract.is("ClientConfigResponse", { ...config, token_scope: "admin" }), false);
  assert.equal(contract.is("ClientConfigResponse", { ...config, channels: [{ id: "1" }] }), false);
  const message = clone(SAMPLES.Message[0]);
  assert.equal(contract.is("Message", { ...message, author_is_bot: "no" }), false);
  // `#219 emoji-reactions`: a tally without its emoji, with a count that is not a count, or a list
  // that is not a list.
  const reacted = clone(SAMPLES.Message[1]);
  assert.ok(Array.isArray(reacted.reactions) && reacted.reactions.length > 0, "no sample carries reactions");
  for (const reactions of [[{ count: 1 }], [{ emoji: "👀", count: "1" }], [{ emoji: "👀", count: -1 }], "👀"]) {
    assert.equal(contract.is("Message", { ...reacted, reactions }), false,
      `Message accepted reactions ${JSON.stringify(reactions)}`);
  }
  for (const value of [null, undefined, "text", 3, []]) {
    assert.equal(contract.is("Message", value), false, `Message accepted ${JSON.stringify(value)}`);
  }
  assert.throws(() => contract.decode("Message", null), /^Error: malformed Message: /);
});

test("an extra field is tolerated, so the server may add one without breaking an open page", () => {
  for (const [root, samples] of Object.entries(SAMPLES)) {
    for (const sample of samples) {
      if (sample && typeof sample === "object" && !Array.isArray(sample)) {
        const extended = { ...clone(sample), a_field_from_a_newer_server: true };
        assert.equal(contract.is(root, extended), true, `${root} refused an unknown extra field`);
      }
    }
  }
});

test("an unknown root is a programming error, not a refusal", () => {
  assert.throws(() => contract.is("NoSuchType", {}), /NoSuchType/);
  assert.throws(() => contract.decode("NoSuchType", {}), /NoSuchType/);
});

test("vibe-talk-v1 frames: an unknown tag is skipped, a malformed known one is refused", () => {
  const decodeFrame = (value) => contract.decodeTagged("VibeTalkV1ServerFrame", value);
  for (const frame of SAMPLES.VibeTalkV1ServerFrame) {
    assert.deepEqual(decodeFrame(clone(frame)), frame);
  }
  // A peer may add frame types; the README says a client ignores the ones it does not know.
  assert.equal(decodeFrame({ type: "ping" }), null);
  assert.equal(decodeFrame({ type: 7 }), null);
  assert.equal(decodeFrame({}), null);
  assert.equal(decodeFrame(null), null);
  assert.equal(decodeFrame("transcript"), null);
  assert.equal(decodeFrame([]), null);
  // A frame whose type IS known has to be well formed. The README says a transcript's role is
  // `user` or `assistant`; a page that guessed at anything else would mislabel who said it.
  assert.throws(
    () => decodeFrame({ type: "transcript", role: "agent", text: "hi" }),
    /^Error: malformed VibeTalkV1ServerFrame: /
  );
  assert.throws(() => decodeFrame({ type: "transcript", role: "user" }), /'text'/);
  assert.throws(
    () => decodeFrame({ type: "transcript", role: "agent", text: "hi" }),
    /\/role must be one of "user", "assistant"$/
  );
  assert.throws(() => decodeFrame({ type: "error", message: 3 }), /\/message must be/);
  // Strict decoding of the whole union still refuses a tag it does not know.
  assert.equal(contract.is("VibeTalkV1ServerFrame", { type: "ping" }), false);
  assert.throws(() => contract.decode("VibeTalkV1ServerFrame", { type: "ping" }), /\/type must be one of/);
  // The per-variant validators behind this are not part of the contract's surface.
  assert.throws(() => contract.is("VibeTalkV1ServerFrame/transcript", {}), /no contract type/);
  // Optional fields really are optional, and an extra one is ignored like the Rust peer ignores it.
  assert.deepEqual(decodeFrame({ type: "turn_complete", later: 1 }), { type: "turn_complete", later: 1 });
  // Only tagged unions can be decoded this way.
  assert.throws(() => contract.decodeTagged("Message", {}), /Message/);
});

test("every root has a declaration the type checker can name", () => {
  for (const root of contract.roots) {
    assert.match(
      DECLARATIONS,
      new RegExp(`\\b(interface|type) ${root}\\b`),
      `web/contract.d.ts does not declare ${root}`
    );
  }
  assert.match(DECLARATIONS, /declare const VibeTalkContract:/);
});

// `el()` in web/voice.js is typed by JSDoc overloads that list the page's form controls by id, so
// the checker knows `.value` and `.checked` exist on them. Those lists are only useful while they
// are the page's real controls: an id missing from them silently types as a plain HTMLElement, and
// an id that no longer exists is a claim about a page this repository does not serve.
test("the el() overloads in voice.js list exactly voice.html's form controls", () => {
  const html = read("web/voice.html");
  const script = read("web/voice.js");
  const TYPE_OF_TAG = {
    input: "HTMLInputElement",
    textarea: "HTMLTextAreaElement",
    select: "HTMLSelectElement",
    button: "HTMLButtonElement",
  };
  const fromHtml = {};
  for (const [, tag, attributes] of html.matchAll(/<(input|textarea|select|button)\b([^>]*)>/g)) {
    const id = /\sid="([^"]+)"/.exec(attributes);
    if (id) {
      (fromHtml[TYPE_OF_TAG[tag]] ||= []).push(id[1]);
    }
  }
  const fromScript = {};
  const overloads = /\/\*\*\s*\n\s*\* @overload\s*\n\s*\* @param \{([^}]*)\} id\s*\n\s*\* @returns \{(\w+)\}/g;
  for (const [, union, type] of script.matchAll(overloads)) {
    if (type !== "HTMLElement") {
      fromScript[type] = [...union.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
    }
  }
  const sorted = (lists) =>
    Object.fromEntries(Object.entries(lists).map(([type, ids]) => [type, [...ids].sort()]));
  assert.deepEqual(sorted(fromScript), sorted(fromHtml));
});
