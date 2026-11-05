---
title: 'agentctl-rust-only: port 100% of the Python edition to Rust, then archive Python'
status: open
priority: 1
issue_type: epic
labels:
- agentctl
- editions
created_at: 2026-10-12T02:30:37.500324332+00:00
updated_at: 2026-10-12T02:30:37.500324332+00:00
---

# Description

[opus 5.5] Owner decision 2026-10-11: every capability of the Python agentctl edition (agentctl, herdr-agent, herdr-subagents, herdr-chat; headless workers, reset/repair/migrate, MCP, the public Google Chat REST transport, chat launch/context/history/adapters) is ported to Rust before py/agentctl moves to archived/ and is marked deprecated. Nothing is dropped, and Rust must be self-contained. Plan: ai_docs/transient/2026-10-11-agentctl-rust-parity/SPEC.md (milestones M0-M9, about 100-130 agent-days) with CRITIQUE.md corrections. Two teams change both editions daily, so new features land in Rust first (and are mirrored into Python until the switch).

# Acceptance Criteria

SPEC.md section 1.1 definition of done, as amended by CRITIQUE.md: every Python command, option, MCP tool, environment variable and file format has a Rust counterpart or an approved documented difference; no Rust binary executes Python; Google Chat works from this repository alone; Rust tests and frozen goldens replace the Python tests and differentials; consumers migrated; Python archived with a deprecation notice.
