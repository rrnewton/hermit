---
title: 'slack-provider: read and post in Slack, several providers in one server'
status: closed
priority: 1
issue_type: feature
labels:
- vibe-talk
- slack
created_at: 2026-10-02T18:34:07.973869078+00:00
updated_at: 2026-10-02T18:34:15.004375299+00:00
closed_at: 2026-10-02T18:34:15.004375169+00:00
---

# Description

[opus 5.5] Add Slack as a third chat integration beside Discord and Google Chat, keep it working with OSS Slack (bot or user token against slack.com/api or a compatible bridge), and let one running vibe-talk serve several configured providers so one served copy covers every chat service.

# Acceptance Criteria

A [[providers]] entry of kind slack reads, threads, registers and posts in Slack; several providers coexist in one instance with per-channel routing and capabilities; deployed on devbig014 against the owner's Slack.
