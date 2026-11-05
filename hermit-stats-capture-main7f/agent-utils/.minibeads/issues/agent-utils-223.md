---
title: 'asset-caching: let the installed app reuse its page files instead of downloading them on every launch'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-08T16:12:35.586733060+00:00
updated_at: 2026-10-08T17:55:02.665356982+00:00
closed_at: 2026-10-08T17:55:02.665356862+00:00
---

# Description

[opus 5.5] Owner, 2026-10-08 18:11 CEST, asked whether the installed PWA re-downloads the page on start. It does: there is no service worker, and every static asset (voice.html, voice.js 262 KB compressed, voice.css, contract.js, style.css) is served Cache-Control: no-store, so each launch fetches all of them. The no-store is deliberate for API answers (channel text belongs to a credential) and must stay there; the page files hold no user data. Wanted: content-hashed asset URLs cached as immutable, and the HTML shell revalidated cheaply (ETag, 304), so a launch costs one small request when nothing changed.
