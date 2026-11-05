# Timeline payload scaling

> **Provenance.** This is a dated investigation record. It was produced against a private
> downstream workspace, so it refers to repositories, hosts and services outside this one
> that cannot be resolved from here. Projects that consume `agent-utils` appear under
> neutral labels — `consumer-a`, `consumer-b` — and those are REDACTIONS, not real names:
> `agent-utils` is a reusable library and has to read standalone, without naming whoever
> happens to use it. **The measurements, dates and findings are unchanged**; only names
> were replaced. Nothing here describes `agent-utils` itself.
> See `#86 scrub-client-names` and `#67 standalone-repo`.

## Measured Consumer A baseline

The 2026-08-12 full-corpus audit measured the generated schema-1 `data/timeline.json` at
196,704,084 bytes. Deterministic gzip level 6 reduced the same bytes to 27,043,927 bytes: 13.7% of
the identity representation, or an 86.3% transfer reduction. This is an important low-risk first
step, but the browser must still download one whole representation, inflate roughly 197 MB, parse
it, and retain the resulting object graph before it can draw any time range.

The audit also found detailed edge `full_text` values duplicating message bodies available through
the phase/detail transcript data. That duplication makes the bootstrap projection carry drill-down
prose which is unnecessary for an outer zoom level. Schema 1 must retain its current fields for
compatibility; a sharded projection should replace those copies with stable message/detail
references and load prose only for the selected range.

## Sharding measurements and recommendation

A schema-2 prototype decomposition put the global bootstrap at approximately 125 KB gzip. Daily
time shards averaged approximately 274 KB gzip, with the largest observed day approximately
827 KB gzip. Those sizes support range-driven loading without making normal pan/zoom interactions
depend on hundreds of tiny requests.

Implement the transition in stages:

1. Keep schema 1 and identity files, add deterministic gzip sidecars, HTTP content negotiation,
   strong validators, and browser revalidation. This is the implemented compatibility step.
2. **Implemented:** add a schema-2 bootstrap containing identity, overall range, aggregate activity
   bins, global lifetime-data reference, and a UTC-day shard catalog with byte sizes and complete
   content digests. Schema 1 continues to be emitted for the CLI and old browsers.
3. **Implemented:** keep structural fork/join edges with global lifetimes, and put detailed
   phases/states, events, and intermediate message edges into daily UTC shards. Fetch the visible
   days plus a small time buffer only at detail zoom, retain resolved shards in a
   `Map<URL, Promise>`, and use immutable digest URLs across reloads. Panning never refetches an
   unchanged shard. A first text query loads all detail shards, preserving global-search semantics
   explicitly instead of searching only already-visible days.
4. **Implemented:** publish a lightweight phase-card index and exact precomputed activity bounds.
   Agent-lifetime drill-down and zoom no longer load every UTC day spanned by a long-lived agent.
5. Keep full transcript/detail documents separately addressable. Remove duplicated edge
   `full_text` from the schema-2 bootstrap and shards once stable references can resolve the same
   content on demand.

The loader now selects schema 2 when its bootstrap is present and falls back to the existing
schema-1 monolith otherwise. This preserves exported archives and basic static-server deployment
while allowing a multi-week view to start from identity, lifetime, and aggregate data rather than
tens or hundreds of megabytes of transcript detail. Real-corpus measurements for the implemented
projection should be refreshed whenever the Consumer A corpus or schema changes; do not substitute the
earlier prototype decomposition for current generated-file measurements.

## Implemented Consumer A measurement (2026-08-12)

The zero-provider projection of the 196,704,084-byte Consumer A schema-1 corpus produced a 31,763-byte
gzip bootstrap, a 941,723-byte gzip global lifetime/structural-edge object, and 24 UTC detail-day
objects totaling 25,896,120 bytes gzip. Day objects ranged from 46,402 bytes to 4,185,274 bytes
gzip, with a 766,772-byte median. Generation took 10.47 seconds and peaked at 1,217,636 KiB RSS;
generator memory is a remaining optimization because the compatibility monolith is parsed before
projection.

With the built-in gzip-negotiating server, a same-host headless-Chromium run measured 1,181,666
encoded bytes, 393.496 ms to a usable aggregate view, and 13,530,928 bytes of used JavaScript heap.
The same corpus through schema 1 measured 196,912,397 encoded bytes without its sidecar, or
27,123,168 bytes with it; both schema-1 cases retained about 214 MB of initial JavaScript heap.
Thus schema 2 reduced initial transfer by 99.4% versus identity and 95.6% versus gzip, and initial
heap by 93.7%. These are single-run engineering measurements rather than performance budgets. The
full reports and a separately serveable package are under
`~/temp/agent-timeline-schema2-measurement/`; no live archive files were changed.

## Outer-zoom interaction measurement (2026-08-12)

The 23-day, seven-team Consumer A archive was measured again after combining coordinator and worker
activity, selecting bins by an eight-pixel minimum, retaining aggregate mode through five minutes
per pixel, and skipping invisible lifetime packing. At the fitted 1440×900 view, resolution changed
from hourly to daily, rendered activity blocks fell from 1,509 to 49, and SVG nodes fell from 3,054
to 135. Usable load time fell from 1,638.485 ms to 301.469 ms. Initial render time fell from 250.1
ms to 53.3 ms; across the deterministic zoom/pan sequence, render p50 fell from 89.4 ms to 47.8 ms
and p95 from 279.6 ms to 57.3 ms. Input-to-animation-frame p95 was 31.566 ms.

The end-to-end initial transfer changed from 197,021,654 bytes to 1,140,452 bytes and initial CDP
heap from 216,223,692 bytes to 12,233,132 bytes. Those two gains include the schema-2 lazy-delivery
work above and must not be attributed solely to the outer-zoom rendering changes. The exact before
and after reports are `qa/full-site-before-summary.json` and
`qa/full-site-after-ui-fixes.json` in the durable Consumer A export.
