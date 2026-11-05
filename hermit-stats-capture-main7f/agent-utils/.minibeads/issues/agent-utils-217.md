---
title: 'markdown-blocks: render lists with round bullets and blank lines as paragraph space, like the Google Chat client'
status: closed
priority: 2
issue_type: feature
created_at: 2026-10-08T15:15:47.059430030+00:00
updated_at: 2026-10-08T22:45:59.448000065+00:00
closed_at: 2026-10-08T22:45:59.447999925+00:00
---

# Description

[opus 5.5] Owner, 2026-10-08 17:14 CEST, two screenshots of one agent message (Google Chat client vs vibe-talk): ours shows '- ' list items as literal dashes with no list indentation, and the blank lines between paragraphs vanish, so the last two lines run together. The page's own renderer (renderMarkdownInto in web/voice.js, no library) draws one block per line with inline styles, code fences and quotes only; an empty line becomes an empty block with no height. Wanted: round-bullet (and numbered) lists with list spacing, blank lines as space between paragraphs, continuation lines kept inside their item.
