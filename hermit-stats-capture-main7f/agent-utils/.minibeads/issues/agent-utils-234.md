---
title: 'oss-redeploy-check: update public deployment and verify portable behavior'
status: closed
priority: 1
issue_type: chore
created_at: 2026-10-11T18:10:26.193076466+00:00
updated_at: 2026-10-11T18:35:10.474324031+00:00
closed_at: 2026-10-11T18:26:25.614270487+00:00
---

# Description

Updated the public vibe-talk deployment from source 8f6fc7104754a5548bfce2c927b936e5598ae867 to current main 002158964114eed96d636490a906b1e771da7236. Built the public Containerfile without internal dependencies or build credentials; deployed image 3b11beccc0d5df0d73772c036ef78e7167851932edfa7812f32c029865518abf. The existing Discord and ElevenLabs configuration runs without a corporate overlay. Authentication credentials and channel configuration were compared before/after and are unchanged.

Validation: the complete 22-step vibe-talk DAG initially passed 19 steps and failed three. All three affected steps were then run separately with their original assertions intact: the complete Rust suite passed 1,207 Rust tests across 38 suite results, including the Node page suites, using the repository-pinned Node 22.23.3; the full screenshot walk passed across phone and desktop profiles; the real Chromium streamed-audio test passed with its slow media-element negative control. The first unit failure was Node 26's changed default test reporter; the host's stale WSL audio socket imposed a measured 5,019 ms AudioContext constructor delay. Excluding that unavailable socket only for the audio test reduced constructor time to 18 ms; streamed speech became audible in 554 ms, and the negative control still took 4,786 ms. The first screenshot attempt timed out in one state; an instrumented reproduction and the complete unchanged rerun passed. No source change or weakened assertion was used to obtain these results. The deployment launcher/container-runtime suite also passed all 79 checks.

Live verification: 11 MCP/read/access-isolation checks passed against the local listener, and the same 11 passed through the configured public tunnel. Read-only browser sign-in on both origins displayed 50 channel messages, with zero uncaught page errors and no horizontal overflow at a 412 px phone viewport. Served HTML shells, scripts and stylesheet match the updated source (shells normalized only for their generated content-addressed asset references). The new container uses the exact built image. Both service and tunnel are active; service restarts remain zero.

Data: a consistent pre-deploy SQLite backup passed quick_check. After deployment all seven historical tables remain present with no reduced row counts; six new tables were added by migrations, and quick_check remains ok. The rollback image and private deployment receipts are retained locally.

Limits: live verification used --skip-post and read-only credentials. No chat messages were sent, no real voice-provider conversation was started, and no physical phone/audio acceptance was claimed. Voice-provider integration, optional Messages API summarizers, and provider configuration behavior were covered by the offline suite rather than by a paid live vendor call.


Separate development-tool findings were recorded as agent-utils-235 (pytest-plugin-identity) and agent-utils-236 (root-terminator-parser). The optional plugin correction passed its focused control but is not included in the deployed app or this record-only publication; its broader gate was blocked by the unchanged parser cases. The required make validate was run and remains RED on the unchanged sharder, under both host pytest 9.0.3 and isolated pytest 8.4.2. The directly run consuming-name and documented-default guards pass; no repository-wide green result is claimed. This publication contains issue records only. No application source or test assertions were modified.
