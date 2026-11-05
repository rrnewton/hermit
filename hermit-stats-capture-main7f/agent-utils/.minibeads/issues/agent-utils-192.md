---
title: 'persist-devbig014-deploy: survive a reboot without falling back to the old build'
status: closed
priority: 2
issue_type: task
labels:
- vibe-talk
created_at: 2026-10-04T10:09:55.559024353+00:00
updated_at: 2026-10-12T19:57:16.968032398+00:00
closed_at: 2026-10-12T19:57:16.968032287+00:00
---

# Description

[opus 5.5] devbig014 runs the current vibe-talk build through a runtime systemd override in /run, chosen because the home directory (units, configs, releases) is shared with devbig030. A reboot drops the override and the app falls back to the previous release. Make the devbig014 deployment durable without changing devbig030's.
