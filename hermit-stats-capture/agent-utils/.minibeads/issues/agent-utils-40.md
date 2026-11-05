---
title: 'typescript-contract: generate shared wire types and validate them at the browser boundary'
status: closed
priority: 1
issue_type: feature
assignee: opus-5.5/typescript-contract
labels:
- vibe-talk
- typescript
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T22:23:23.152115702+00:00
updated_at: 2026-09-26T00:23:27.906935824+00:00
closed_at: 2026-09-26T00:23:27.906935715+00:00
claimed_at: 2026-09-25T22:23:25.488886715+00:00
claimed_until: 2026-09-26T22:23:25.488737211+00:00
---

# Description

[opus 5.5] Implement the approved recommendation in `ai_docs/transient/2026-09-24-vibe-talk-typescript-shared-types.md` (#140 typescript-shared-types). Rust DTOs stay authoritative; a `contract` module derives `schemars::JsonSchema`; a generator writes deterministic JSON Schema, TypeScript declarations, and standalone Ajv validators that are checked in and verified by a staleness check. `tsc` checks the existing browser script with `allowJs`/`checkJs`/`noEmit` (no bundler, no Wasm), and network/storage boundaries decode `unknown` through the generated validators before the page trusts a value.

# Acceptance Criteria

Generated schema, declarations, and validators are deterministic, checked in, and fail validation when stale; tsc checks the browser script; key HTTP, WebSocket, live-stream, and cached payloads are decoded through generated validators; existing browser, offline-cache, and voice-health behaviour is unchanged; make validate passes and the commit is on public main.
