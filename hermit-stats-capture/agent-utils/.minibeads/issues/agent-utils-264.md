---
title: 'voice-send-approval: a Settings switch for approving voice-agent sends, confirmed by voice or tap when on, direct when off'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-13T00:24:34.624786918+00:00
updated_at: 2026-10-13T07:33:39.292792233+00:00
closed_at: 2026-10-13T07:33:39.292792092+00:00
---

# Description

[opus 5.5] Owner, 2026-10-13 00:22Z: "Hitl confirmation of sending a prompt by the voice agent ... This should be a check box in the settings for human approval of voice agent sends. And yes, we should certainly be able to confirm verbally the whole point is hands free usage. So much like Siri does. When I ask it to send a message it will end the question with "send it?" And I say yes and it sends. But this is actually not my preferred workflow. I will turn this option off because when I use [another voice coding client] I just tell the agent to tell the coding agent something and it does it. I don't want extra friction."

Build: a Settings checkbox "Ask me before the voice agent sends" (on by default in the shipped config). On: post_reply proposes, the agent asks "Send it?", and a spoken yes (judged outside the model, after an uninterrupted read-back) or a tap on Send posts it. Off: post_reply posts at once and the agent says it was sent only after the tool reports it. The voice-agent prompt follows the setting (a third variant), so the agent never asks for a confirmation that is not needed or claims a send that did not happen.
