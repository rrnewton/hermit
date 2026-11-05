---
title: 'dagrun-includes: compose validation DAG fragments at load time'
status: closed
priority: 0
issue_type: task
labels:
- dagrun
- validation
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T04:10:39.786278915+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Add a load-time include mechanism with namespaces to Python and Rust dagrun, then use it to assemble tool-owned validation DAG fragments.

# Acceptance Criteria

The reserved include key is never silently ignored; malformed, absolute, escaping, cyclic, duplicate, or conflicting includes fail explicitly; Python and Rust behavior is equivalent; path-aware loaders and every CLI command accepting `--dag` consume the same flattened graph, while context-free parsers reject includes; nested fragments are exercised by parity tests.

# Outcome

[gpt-5.6-sol] Closed with strict Python/Rust load-time include parity. Includes flatten transparently under composable namespaces; `include.after` fans outer prerequisites into every included entry node, and callers may target exact namespaced internal nodes. Includes are not synthetic runtime nodes and do not imply a whole-fragment completion edge: a fragment that needs node-like completion must expose an explicit terminal barrier depending on all intended exits. Missing, absolute, escaping, malformed, cyclic, duplicate, conflicting, or over-limit composition fails before planning, and path-free/stdin parsing rejects the reserved key rather than ignoring it.
