---
title: 'floating-search: the search glass floats beside the freshness pill instead of costing a header row'
status: closed
priority: 2
issue_type: feature
created_at: 2026-10-04T11:34:31.447406814+00:00
updated_at: 2026-10-04T12:59:22.260506192+00:00
closed_at: 2026-10-04T12:59:22.260506071+00:00
---

# Description

[opus 5.5] Owner feedback 2026-10-04 07:25, from an Android phone screenshot of the main channel view: "Wasteful search bar. The little search button in the upper right wastes a full horizontal strip of space. It should just be a floating icon which would put it on the same vertical height as the refresh status floating widget." In the screenshot the header row (~2.4rem plus the status-bar inset) holds nothing but the magnifier at its right edge; the list starts under it, with the floating freshness pill centred over the top of the list. Plan: (1) on the main screen the header takes no row at all when the control bar is in the dock (the default) - the glass leaves the header and becomes a small round button floating over the top-right corner of the list, in the same zero-height anchor idiom as the pill, centred on the pill's own vertical centre and drawn with the pill's panel/edge/shadow, its visible disc the pill's height while the hit area stays >= 44px; (2) opening search grows that button into a floating bar across the top of the list - field, count and the glass as its close control - which is the one place the on-screen keyboard never covers; the pill gives way while it is open, the way it already gives way to a pull, and the list reserves the bar's height at its head so the first match is never under it; closing (glass or Escape) collapses it and clears the query, as before; (3) Settings, Reply and Help keep their title bar with a way back, and a reader who moved the bar to the top keeps it there - the glass floats in both placements, so the bar no longer has to yield the header row while searching; the thread heading is above the list's top and cannot collide with either floating control; with no header on screen the frame applies the top safe-area inset itself; (4) page tests for the hidden header, the floating glass, open/close and filtering, and real-Chromium geometry in tests/offline_cache_browser.py (no empty header strip, glass centred on the pill and not overlapping it, Settings title bar intact).
