# TypeScript and shared wire types for vibe-talk

Status: analysis, approved. Steps 1-3 and the generated-output gate of step 5 are implemented; see
`2026-09-25-vibe-talk-typescript-contract.md` for what shipped and what remains.

Tracking: #140 typescript-shared-types

## Current shape

The browser is a plain JavaScript application: `web/voice.js` is loaded directly by the page and
uses browser APIs without a bundler or module loader. Rust serves the HTTP API and chooses the
conversational voice provider. No Rust or WebAssembly runs in the browser.

The main client file is about 9,800 lines and its JavaScript fixture is about 16,500 lines. The
fixture provides strong behavioral coverage, but it cannot statically prove that a property read
from an API or WebSocket frame exists. Rust has typed response structures for the HTTP endpoints,
while the browser independently spells their JSON field names. The public `vibe-talk-v1`
WebSocket protocol is similarly described in prose and implemented independently by its peers.

The resulting risks are concentrated at boundaries:

- a Rust field can be renamed or made optional while JavaScript still assumes the old shape;
- tagged unions such as live-ingest changes and WebSocket frames are switched on as strings;
- route query parameters and response payloads are not connected in the type system;
- TypeScript alone would still trust malformed network JSON because its types disappear at
  runtime.

## What TypeScript would change

TypeScript would improve local refactors of view state, DOM helpers, API calls, and the two voice
protocol handlers. It would catch misspelled fields and missing union cases before the browser
test suite runs. It would not replace the existing behavioral tests, browser compatibility checks,
or runtime validation of network data.

A migration also introduces a compiler step. The current static assets can be inspected and served
as written. A TypeScript source file must produce JavaScript before the Rust binary embeds it. The
smallest compatible toolchain is `tsc` without a bundler: keep one browser script, compile it to
the existing JavaScript target, and verify that generated output is current. ES modules and a
bundler can follow later if splitting the file proves valuable; they are not prerequisites for
types.

## Options for sharing Rust and browser types

| Option | Strengths | Limits | Fit here |
|---|---|---|---|
| Handwritten TypeScript interfaces | No Rust dependency or generator; easy first step | Two sources of truth; drift remains possible | Useful only as a temporary migration aid |
| `ts-rs` or `typeshare` derives | Direct TypeScript declarations from Rust `serde` data types; small conceptual surface | Describes payloads, not routes; generated types do not validate JSON at runtime | Good if the goal is compile-time sharing only |
| `schemars` JSON Schema plus TypeScript generation | Language-neutral checked-in contract; can also drive runtime validation and non-Rust bridge conformance | Requires a generation pipeline; schema details around optional fields and tagged enums need tests | Best base for HTTP and WebSocket payloads that have several implementations |
| OpenAPI generated from Rust, then `openapi-typescript` | Shares HTTP routes, methods, query parameters, errors, and bodies as one contract | Axum handlers need route/schema annotations; WebSockets still need a separate schema; runtime checks remain separate | Best if typed HTTP calls are the main objective |
| A TypeScript-first schema library generating Rust | Excellent browser runtime validation | Moves authority away from the existing Rust DTOs and complicates the Rust build | Poor fit for this server |
| WebAssembly exports from Rust | Can reuse Rust code directly | Large build/runtime change; does not by itself type ordinary browser and DOM code | No benefit for this migration |

## Recommended contract

Keep Rust as the source of truth for wire data and generate a portable schema artifact from a
small contract module.

1. Move public request and response DTOs into a `contract` module. They continue deriving
   `Serialize` and `Deserialize`, and additionally derive `JsonSchema` with `schemars`.
2. Define the public `vibe-talk-v1` client and server frame enums in that same public module as
   `serde` tagged unions. A deployment-managed bridge can test its frames against the checked-in
   schema without importing deployment details into this repository.
3. Generate deterministic JSON Schema files and TypeScript declarations. Check both into the
   repository initially, with a validation command that regenerates them and fails on a diff.
   This keeps ordinary Rust builds independent of an npm registry and makes contract changes
   reviewable.
4. Add runtime decoders at network boundaries. An Ajv-based decoder generated from the same JSON
   Schema is the most direct option. A smaller handwritten decoder is acceptable only if a test
   proves every generated union variant is handled; a type assertion such as `as TimelineResponse`
   is not validation.
5. Keep route construction in a thin typed API module. JSON Schema shares payloads. If route/query
   drift becomes the dominant problem, add OpenAPI generation later rather than adopting its
   annotation cost before it is needed.

This split gives the important guarantee: Rust serialization, generated TypeScript, runtime
decoding, and bridge conformance all describe the same tagged unions. It avoids making the browser
build or one private bridge the authority for the public protocol.

## Suggested migration sequence

### 1. Measure before converting

Add a `tsconfig` that checks the existing JavaScript with `allowJs`, `checkJs`, and `noEmit`.
Annotate only boundary helpers and state objects with JSDoc. Record the initial diagnostic classes;
do not silence them with broad `any` types.

### 2. Establish generated contracts

Start with the highest-value shapes:

- `ClientConfigResponse`, `VoiceDescription`, and `VoiceSession`;
- `Message`, `MessageThread`, `ThreadSummary`, and `TimelineResponse`;
- the standard API error body;
- `vibe-talk-v1` client and server WebSocket frames;
- live stream events.

Add serialization fixtures for optional fields and every tagged-union variant. Generation must be
deterministic and must preserve `snake_case`, `flatten`, and omitted optional fields exactly as
`serde` emits them.

### 3. Create one typed boundary module

Introduce typed `api()` wrappers and WebSocket decoders while the rest of the page remains
JavaScript. Network values enter as `unknown`, pass runtime validation once, and only then become a
generated type. This step captures most of the safety value without rewriting rendering code.

### 4. Convert by behavior area

Rename and convert coherent sections rather than the entire 9,800-line file at once: shared state
and DOM helpers, channel timelines, outgoing messages, voice transports, then settings. Keep each
conversion behavior-neutral and run the existing fixture after every slice.

If modules are introduced, split along those same boundaries. Until then, `tsc` can emit one
script and preserve the current loading model.

### 5. Make generated output and strict checking release gates

The final gate should run the schema generator, reject a dirty generated tree, compile with
`strict`, and execute the existing browser fixture against the emitted JavaScript. The container
or release build must embed that verified output. A source-only TypeScript check is insufficient if
the deployed JavaScript can be stale.

## Decisions to make before implementation

- Whether generated JavaScript is committed or built in the release image. Committing it keeps
  deployment independent of npm but adds generated diffs; building it avoids those diffs but adds
  Node and package installation to the release path.
- Whether runtime validation uses Ajv or focused generated decoders. This page handles
  authenticated but still untrusted third-party content, so compile-time types alone are not an
  adequate boundary.
- Whether HTTP route metadata merits OpenAPI in the first iteration. Payload-only schema covers
  the current drift risk with fewer annotations; OpenAPI becomes worthwhile when typed route
  construction or external HTTP clients are required.
- Which protocol changes are backward compatible. Additive optional fields are generally safe;
  tag changes, required fields, and changed meanings need a new protocol revision or explicit
  capability negotiation.

## Recommendation

Use `schemars` plus generated TypeScript declarations and runtime validators, with Rust DTOs as the
authoritative source. Begin with `checkJs` and a typed boundary module, then convert the browser in
small sections. Keep `tsc` as the initial compiler and defer a bundler until modules provide a
measurable maintenance benefit. Do not use Wasm for this work: it would add a second runtime and a
larger deployment change without solving DOM typing or network validation.
