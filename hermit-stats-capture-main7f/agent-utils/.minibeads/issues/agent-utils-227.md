---
title: 'github-link-abbrev: show bare GitHub URLs in message text as short references like repo#123'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-08T22:44:54.614019930+00:00
updated_at: 2026-10-09T00:10:50.325598195+00:00
closed_at: 2026-10-09T00:10:50.325598075+00:00
---

# Description

Owner 2026-10-08: agent status messages are full of bare https://github.com/<owner>/<repo>/pull/<n> URLs. Preprocess message markdown before it becomes HTML (a separate fork from the voice TTS preprocessing): a bare GitHub link renders as a short hyperlink, e.g. repo#3871, keeping the full URL as the target.
