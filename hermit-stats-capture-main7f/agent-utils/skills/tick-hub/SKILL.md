---
name: tick-hub
description: One scheduled tick that funnels many recurring responsibilities, each on its own cadence, into machine-readable HEALTH/ACTION/NOTE/ERROR lines for a coordinator or automation to dispatch. Use when consolidating N separate timers/cron reminders into a single heartbeat, or wiring cadenced gates and freshness health checks that emit parseable output.
---

# tick-hub

A single scheduled "tick" (a cron job, coordinator loop, or systemd timer) checks every DUE
reminder — plain timed, flag-gated, or shell-gated with captured values — plus file-freshness
health checks, and emits a stable, line-oriented `HEALTH:` / `ACTION:` / `NOTE:` / `ERROR:` report.
One loop carries every recurring responsibility instead of N separate timers. Reminders, gates, and
health checks are all caller config; the engine is pure and deterministic given `now`.

The CLI is the source of truth for usage — do not rely on this file for details. Run:

- `tick-hub quickstart` — self-contained getting-started tour (no repo needed).
- `tick-hub --help` — commands and flags.
- `tick-hub --userguide` — the full user guide (complete reference).
