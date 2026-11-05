---
title: 'public-path-hygiene: replace host-specific and internal paths in public docs with public examples'
status: closed
priority: 3
issue_type: chore
labels:
- vibe-talk
- docs
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-26T10:29:04.019315230+00:00
updated_at: 2026-09-26T10:36:46.652315000+00:00
closed_at: 2026-09-26T10:36:46.652315000+00:00
---

# Description

[opus 5.5] Found in the final doneness audit for #1 mobile-chat-reader. Three public documents name paths that only exist on one machine or inside one organisation, so a reader cannot resolve them:

- vibe-talk/README.md: the example systemd unit's `Documentation=` line points at a file under one person's home directory.
- vibe-talk/RELATED_WORK.md: the Happy Coder entry names the directory where a checkout sits on the review host.
- reviews/research/wrkviz-comparative-analysis.md: the list of coordinators names one by its path inside a private monorepo.

# Acceptance Criteria

Each path is replaced by a public or provider-neutral equivalent that keeps the sentence's meaning: the unit still links to this README, the related-work entry still says a local checkout was reviewed and names the public fork, and the analysis still lists an in-house orchestrator among the coordinators. make validate passes.

# Notes

[opus 5.5] Delivered in 664b60c on main. The unit's Documentation= now links to vibe-talk/README.md on GitHub. The Happy Coder entry says a local checkout of the reviewed revision was read, and still names the public fork. The analysis names "an in-house orchestrator" in all three places. make validate passed (20 of 20 selected checks). Other host paths elsewhere in the tree, such as the test fixture cwd values in py/tests/test_timeline_identity.py, were outside this task's three findings and are unchanged.
