---
title: 'voice-chat-tools: let the internal agent read configured chat (read-only)'
status: closed
priority: 0
issue_type: feature
assignee: opus-5.5
labels:
- vibe-talk
- voice
- chat
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T03:55:15.307829973+00:00
updated_at: 2026-09-25T23:54:35.158742000+00:00
closed_at: 2026-09-25T23:54:35.158742000+00:00
claimed_at: 2026-09-25T05:09:54.970802427+00:00
claimed_until: 2026-09-27T05:09:54.970716288+00:00
---

# Description

Make the provider-neutral conversational voice interface expose the chat read and search capabilities to the selected voice provider, and prove that the internal agent can summarize real configured chat messages.

**Delivered scope is read-only.** The internal provider's bridge is given only a read-scope credential, so the application never lists the posting tool to it. Writing from voice needs a confirmation gate that the model cannot bypass. The application currently asks for spoken confirmation only in the tool description, and relies on providers with a per-tool approval mode to enforce it. That write path is split out as #34 voice-chat-write-confirm, and no voice path receives write scope until it lands.

# Acceptance Criteria

One live acceptance on the final deployed voice stack, using the read-only credential:

- Asked to list its tools, the internal agent names the read tools and no posting tool.
- Asked about a real configured channel, it gives a summary that matches that channel's actual recent messages.
- Asked explicitly to send a message, it declines or says it cannot post, and nothing is posted.
- No message is posted at any point during the acceptance. This is verified from the application's post records and the channel's message count before and after.

The public record keeps only content-free evidence (tool names, pass/fail, counts). Message content stays in the private deployment record.
