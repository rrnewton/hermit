# vibe-talk typed wire contract: what shipped

Tracking: #140 typescript-shared-types, #agent-utils-40 typescript-contract

Implements the recommended contract from `2026-09-24-vibe-talk-typescript-shared-types.md`. The
settled operator-facing description is the README's "The wire contract".

## What shipped

- **Rust is the source.** `src/contract.rs` holds the response and frame types that used to be
  ad-hoc `json!` values or private structs: the client-config answer (now with `LiveDelivery` and
  `TokenScope` enums instead of strings), the timeline page, the API error body, the three live
  events, and both directions of `vibe-talk-v1`. The model types they carry derive `JsonSchema`.
  `live.rs` and `speech.rs` serialize the typed values, so the schema describes what is sent.
- **Generated and checked in.** `tests/contract.rs` writes `contract/vibe-talk.schema.json` and
  `contract/samples.json` (`VIBE_TALK_UPDATE_CONTRACT=1`) or fails when they are stale.
  `contract/generate.mjs` turns the schema into `web/contract.js` (Ajv 8 standalone validators,
  wrapped as a classic script exposing `VibeTalkContract`) and `web/contract.d.ts`. Its `--check`
  mode is the staleness gate. Ajv and TypeScript are pinned by `contract/package-lock.json`.
- **Validated at untrusted boundaries only.** Client config, voice-session mints, and timeline
  pages decode strictly; live `message` events are checked, falling back to a re-read; server
  `vibe-talk-v1` frames use `decodeTagged` (unknown tag ignored, known tag malformed refused);
  saved `Message`/`ThreadSummary` rows are checked when read back from device storage.
- **checkJs, no build.** `tsconfig.json` checks `web/voice.js` with `allowJs`, `checkJs`, `noEmit`
  against the generated declarations and a two-line `web/globals.d.ts`. No bundler, no emitted
  JavaScript, no Wasm; the page still loads its checked-in scripts directly.
- **Tests.** `tests/js/contract.test.mjs` runs every sample through the generated validators, drops
  every required field of every sample and expects a refusal naming it, covers tag handling, and
  keeps `el()`'s JSDoc overloads equal to `voice.html`'s form controls. The page fixture now loads
  `contract.js`, so every fake server answer in it has to conform.

## checkJs baseline

Measured on `web/voice.js` at the start of this work with TypeScript 5.9.3:

| Mode | Errors | Classes |
|---|---:|---|
| non-strict `checkJs` | 128 | 126 TS2339 (property does not exist), 2 TS2551 |
| `strict` | 1,533 | 413 TS2531, 401 TS7006, 215 TS2339, 210 TS7005, 138 TS18047, remainder smaller |

Non-strict is now **0** and gated. Almost all of the 128 were `document.getElementById` returning
`HTMLElement` where the code reads `.value`, `.checked`, or `.disabled`; typed `el()` overloads and a
few casts at `querySelector`/`event.target` sites cleared them without `any`.

## Behaviour changes

- A client-config, voice-session, or timeline answer missing a required field is refused with a
  "reload the page" error instead of being read with fallbacks. Page and server ship in one binary,
  so this can only be a stale open tab or a server bug. The page's older-server fallbacks remain in
  place but are no longer reachable through the decoders.
- A `vibe-talk-v1` frame with a known `type` but an off-spec shape (for example a transcript whose
  `role` is not `user` or `assistant`) is now an error rather than best-effort, reported to voice
  health as `error_frame` and disarming the no-reply bound exactly as an `error` frame does. A peer
  that follows the README is unaffected. A new enum value (a role, scope, or delivery mode) is
  therefore a protocol change for an older page; see the `contract` module docs. `tests/read_aloud.rs`' mock peer had sent `role: "agent"` and was
  corrected.

## Not done

- `strict` is not enabled; the 1,533 strict diagnostics are the next slice (step 4/5), by
  behaviour area.
- Routes outside the listed roots (summaries, replay, transcript store, to-do, directory, and
  others) are still untyped on the page.
- `applyTimelinePage` stays untyped because it also takes saved cache entries with page-local fields.
- No OpenAPI or route metadata; payload schemas only.
