---
title: 'post-split-per-provider: split a long post only at the provider''s own limit, and put the extra parts in a thread under the first'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T22:39:41.315349693+00:00
updated_at: 2026-10-13T05:29:09.764983263+00:00
closed_at: 2026-10-13T05:29:09.764983122+00:00
---

# Description

Owner 2026-10-12: a long message sent from vibe-talk to a Google Chat space arrived as THREE messages. The server splits every post with split_for_discord (2000 chars) for every provider (src/ops.rs reply_scoped). Google Chat's documented cap is 32,000 bytes and Slack's far above 2000, so split only at each provider's real limit; and where a split is still needed and the provider has threads, post the first part in the main conversation and the rest as replies in its thread.
