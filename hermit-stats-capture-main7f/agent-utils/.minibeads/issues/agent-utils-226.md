---
title: 'page-energy-profile: profile the installed app''s CPU and memory, find hot spots, check for regressions'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-08T16:13:57.675339806+00:00
updated_at: 2026-10-09T10:08:04.900647769+00:00
closed_at: 2026-10-09T10:08:04.900647588+00:00
---

# Description

[opus 5.5] Owner, 2026-10-08 18:13 CEST: he uses vibe-talk as an installed PWA on desktop Chrome and on a phone and wants to be sure it is not draining the battery. Use CPU (and memory) as the energy proxy: profile idle, scrolling, polling/live updates, read-aloud and view switches in real Chromium (and a mobile emulation profile), identify hot spots (timers, polling, layout thrash, observers, re-renders) and optimize. Schedule AFTER every build in flight has landed, so it also catches regressions they introduced.
