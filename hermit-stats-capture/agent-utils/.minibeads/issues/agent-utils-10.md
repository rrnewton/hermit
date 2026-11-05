---
title: 'agent-space-linking: support or document one-space-per-agent linking'
status: closed
priority: 2
issue_type: task
assignee: opus-5.5
labels:
- agentctl
- chat
- design
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:03.478573636+00:00
updated_at: 2026-09-25T06:44:20.680029047+00:00
closed_at: 2026-09-25T06:44:20.680028947+00:00
claimed_at: 2026-09-25T04:48:21.466739184+00:00
claimed_until: 2026-09-27T04:48:21.466611662+00:00
---

# Description

[gpt-5.6-sol] Determine whether one managed agent can be linked directly to its own chat space instead of requiring a thread in a shared space, then implement or document the supported mapping model.

# Acceptance Criteria

Capabilities and constraints are documented from real bridge behavior; either a per-agent space can be linked and verified end to end, or the exact blocker and best supported alternative are recorded.

# Notes

[opus 5.5] Outcome. A managed agent can own its own chat space: one space, one agentctl chat bridge, one agent pane, and one state directory per agent. Every thread in the space routes to that agent. A per-agent thread inside a shared space is not supported, because the bridge routes by space only and two bridges on one space would both ingest every thread. Both chat guides now state this. Evidence: a fresh probe space linked to a freshly started managed Claude agent delivered the owner's message to that agent alone, and the bridge captured its tagged reply once #20 claude-linux-reply-bullet was fixed. An earlier dedicated-space deployment completed a full ACK plus threaded-reply round trip. The private deployment runbook now documents the operator workflow. Unverified: a live outbound reply in the probe space, because that deployment's current transport is deliberately inbound-only; outbound rests on the earlier round trip. The probe agents, bridge, configs and state were removed. The probe space awaits the owner's confirmation to delete, because the provider CLI does not let an agent confirm deletion.
