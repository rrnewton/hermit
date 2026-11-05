---
title: 'auto-read-noise: treat trivial placeholder messages as read automatically'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-04T12:19:56.425434330+00:00
updated_at: 2026-10-04T12:59:22.256991072+00:00
closed_at: 2026-10-04T12:59:22.256990972+00:00
---

# Description

[opus 5.5] Owner request, 2026-10-04, verbatim: "Let's auto Mark as read messages that match a certain trivial filter. Initially populate this with "Working..." Messages. Those are noise."

Evidence: in the owner's main space, 9 of the latest 50 messages are exactly `_Working…_` (markdown italics around "Working" plus U+2026), posted by a bot account as thread replies; the real answer arrives later as a separate message.

"Read" here means vibe-talk's OWN local read/Done state. Nothing in this touches the source chat service's read cursor (the upstream-read route is not involved).

Design:

1. A server-side list of noise rules, kept in the store so it survives a restart and is shared by every device and the voice agent. Until the owner saves a list of his own the shipped default stands in (one rule, `Working…`); a read never writes it. Read through client-config (`noise_rules`), replaced whole through `PUT /api/v1/noise-rules` (write scope). No MCP tool can change it, for the channel-alias reason: a model must not be able to hide messages from the owner.
2. A deliberately trivial matcher, not a regex: normalise both sides (trim; strip surrounding `_ * ~`; collapse whitespace; case-insensitive; `...` and `…` equal) and compare the WHOLE text; a rule ending in `*` matches as a prefix.
3. Evaluated on current content every time messages are served (page, timeline, messages, live stream, todo, count, digest, the agent's tools), never by writing dismissals. Served messages carry `noise: true`, so the page uses the server's one predicate. An edited message that stops matching is unread again; removing a rule un-hides its messages.
4. Everything that counts or reads to-do treats noise as read: `/todo` leaves it out, `/count` and `/digest` skip it and say how many, the agent's digest/page/count/find tools skip it, read-aloud neither prepares nor relays it, the page gives it no unread styling and hides it under Hide read, and shows it dimmed otherwise.
5. Override: "Not noise" on a row records a per-message exemption in the store (`POST /api/v1/channels/{id}/not-noise`), bounded like dismissals.
6. Settings: an "Automatically read" group with the rule list, add, remove, the server's one-line matching rule, and a count of matching loaded messages.
