---
title: 'voice-socket-open: some voice starts never open the voice socket'
status: closed
priority: 2
issue_type: bug
created_at: 2026-10-07T09:26:25.765835410+00:00
updated_at: 2026-10-13T02:44:47.063337145+00:00
closed_at: 2026-10-13T02:44:47.063337055+00:00
---

# Description

[opus 5.5] Found while investigating #190 voice-agent-tools: 11 voice starts from the page (2026-10-02 16:51-16:52 and 20:48, 10-03 17:47, 10-04 08:10 UTC) logged voice_startup with session_acquired and microphone_ready but socket_open=- and provider_ready=-, so the call never connected. Not investigated yet: find why the socket does not open (bridge down, same-origin /ws proxy, page state) and make the page say so.
