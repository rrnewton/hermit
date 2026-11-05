---
title: 'android-pwa-install: make Chrome offer a real app installation'
status: closed
priority: 1
issue_type: task
assignee: gpt-5.6-sol
labels:
- vibe-talk
- pwa
- android
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:02.256665735+00:00
updated_at: 2026-09-25T03:01:31.512308143+00:00
closed_at: 2026-09-25T03:01:31.512307222+00:00
claimed_at: 2026-09-25T02:27:21.662683343+00:00
claimed_until: 2026-09-27T02:27:21.662543043+00:00
---

# Description

[gpt-5.6-sol] Make the mobile reader installable by Chrome Android while preserving browser-mode fallback for platforms where standalone microphone behavior is risky. Current Chrome diagnostics report manifest-display-not-supported because the manifest deliberately selects browser display.

# Design

Chrome 151 diagnostics report manifest-display-not-supported for display browser. Keep browser as the fallback and add display_override standalone; Chrome Android supports it while current Safari does not. Existing HTTPS, start URL, scope, and 192/512/maskable icons are valid; a service worker is not required for installation.

# Acceptance Criteria

Current Chrome Android reports no manifest display installability error and offers Install app; the chosen manifest fallback remains explicit; tests and operator documentation describe Android and iOS behavior.
