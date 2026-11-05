---
title: 'container-prompt-files: the vibe-talk image build cannot find the voice-agent prompts'
status: closed
priority: 1
issue_type: bug
assignee: gpt-5/container-prompt-files
labels:
- vibe-talk
- containers
- correctness
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-29T19:42:00.000000000+00:00
updated_at: 2026-09-29T19:42:45.969865299+00:00
closed_at: 2026-09-29T19:42:45.969865299+00:00
claimed_at: 2026-09-29T19:42:00.000000000+00:00
claimed_until: 2026-10-01T19:42:00.000000000+00:00
---

# Description

[gpt-5] `src/voice_agent.rs` compiles three prompt files into the server with `include_str!`, but
the image build stage copies only `src/` and `web/`. A normal checkout therefore builds while
`podman build -f vibe-talk/Containerfile vibe-talk` fails when Rust tries to read
`prompts/voice-agent-system.txt`, `prompts/voice-agent-send.txt`, and
`prompts/voice-agent-read-only.txt` from the container context.

# Acceptance Criteria

The build stage copies every non-test file consumed by `include_str!` or `include_bytes!`. A
regression test inventories those compile-time inputs and fails with the missing paths if a COPY
source is removed. `make validate` passes, and the container compile stage builds successfully.

# Notes

[gpt-5] Delivered in 98872e88 on main. `make validate` passed all 21 selected checks. An offline
Podman build completed the build stage and exported the 16 MiB server binary. Removing
`COPY prompts ./prompts` makes the regression name all three missing prompt inputs.

The delivery commit mistakenly says `#51 container-prompt-files`; #51 reply-view was already
allocated on GitHub. This record and its GitHub issue use the correct identity,
#178 container-prompt-files. Shared history was not rewritten.
