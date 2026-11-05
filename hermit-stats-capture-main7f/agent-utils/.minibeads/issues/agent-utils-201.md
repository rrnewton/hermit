---
title: 'speech-prep-linear: spoken-text preparation is super-linear on very long lines'
status: closed
priority: 3
issue_type: bug
created_at: 2026-10-04T14:01:18.899331331+00:00
updated_at: 2026-10-12T22:11:39.667932244+00:00
closed_at: 2026-10-12T22:11:39.667932104+00:00
---

# Description

[opus 5.5] Found 2026-10-04 while fixing #200 reply-context: for_speech on a 210,000-character line took 142 s in a debug build. strip_inline was quadratic (it copied the rest of the line at every character) and is fixed; another stage (speak_times, name_opaque_strings or collapse_whitespace) is still super-linear. Real messages are capped near 4,000 characters by the providers, so this is low priority, but a stranger's text should not cost the square of its length.
