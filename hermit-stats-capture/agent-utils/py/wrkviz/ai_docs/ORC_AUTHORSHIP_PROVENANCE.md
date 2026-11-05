# Orc authorship provenance

> **Provenance.** This is a dated investigation record. It was produced against a private
> downstream workspace, so it refers to repositories, hosts and services outside this one
> that cannot be resolved from here. Projects that consume `agent-utils` appear under
> neutral labels — `consumer-a`, `consumer-b` — and those are REDACTIONS, not real names:
> `agent-utils` is a reusable library and has to read standalone, without naming whoever
> happens to use it. **The measurements, dates and findings are unchanged**; only names
> were replaced. Nothing here describes `agent-utils` itself.
> See `#86 scrub-client-names` and `#67 standalone-repo`.

## Finding

An Orc `role=user` block is not necessarily owner-authored. Orc preserves useful
ingress metadata, but a `Submitted` Web or TUI source does not identify the
sender:

- `user_source.Orc.sender_session` is intrinsic inter-agent provenance.
- `user_source.GChat` can intrinsically identify an owner or another human.
- `user_source.Submitted.source` identifies Web/TUI ingress only. The observed
  `extra` object contains prompt hints, not sender identity.

Consequently, Web/TUI submissions without intrinsic provenance, a successful
cross-log delivery join, or an explicit audited channel-ownership rule must
remain unknown. Their text may look human-authored or agent-authored, but style
is not proof.

## Audited examples

### Web submission `v2-message-6045`

The Orc block is `v2-block-430086` in coordinator session
`1b176094-9503-4d6a-ac4a-32af5cb45c16`. It was created at
`2026-08-10T09:18:47.849Z` with:

```json
{"Submitted":{"source":{"Web":{"view":"Unspecified"}}}}
```

`extra` is null. Neither the Orc row nor the retained event/workflow records
name a sender.

Cross-provider evidence does. Codex session
`019fe949-ce0c-7c81-bd47-57db7ce7d4b5` invoked
`scripts/orc-consumer-a-msg.py` at `09:18:18.882Z` with bytes exactly equal to the
Orc block. The delivery completed at `09:18:22.240Z` with an API-accepted HTTP
200 receipt. The Orc block followed 25.609 seconds later.

- UTF-8 bytes: 712
- SHA-256: `b6b659fd841ec09dbdc1d9d37fbef86dab5f4ea75fabdfbba787b42587ca8d60`
- Equality: byte-for-byte, not fuzzy text similarity

This proves the provider session that submitted the message. The friendly label
`codex-closure` appears in the message and surrounding records, but that label
is weaker than the provider session identity unless a separate historical
agent-name mapping proves it.

### TUI submission `v2-message-4409`

The relevant occurrence is `v2-block-1435500` in coordinator session
`4fb50e87-5d91-4294-88b2-afeedf6cc917`, created at
`2026-08-03T03:25:23.680Z` with:

```json
{"Submitted":{"source":"Tui"}}
```

Claude session `51faa3b0-6472-4116-b69a-aa86d8de0ebc` invoked the same relay
tool at `03:25:04.465Z` with exactly the Orc block bytes. Its successful tool
result at `03:25:08.637Z` records delivery to the intended Orc coordinator TUI;
the Orc block followed 15.043 seconds later.

- UTF-8 bytes: 575
- SHA-256: `37a5a253bf0870c21e7669eeb2acda952b3100a3b0b7915feafd2b776406a124`
- Equality: byte-for-byte

The message calls itself `consumer-a-gate`, but the durable proof is that this
Claude provider session sent it. Coordinator-assigned names may be reused, and
self-labels are not identity credentials.

## What other Orc records prove

- `events.db` user-message payloads observed in this lineage contain the text
  and a broad source such as `web`; their event `source` is empty and they do
  not carry the originating provider session or the Orc native message ID.
- `workflows.db` identifies scheduled workflow definitions and runs. It can
  establish a workflow origin when a specific workflow emitted a message, but
  it does not attribute arbitrary Web/TUI relay submissions.
- `v2-message-N` is not a globally unique identity. The long-running audited
  session reuses `v2-message-4409` for multiple unrelated blocks after counter
  resets. The append-only `content_blocks.id` is the unique Orc occurrence.
- Timestamps are corroboration, not identity. Separate queued messages can have
  the same `created_at_ms`, and delivery-to-ingest delay is variable.
- The relay's own append-only delivery log records a content hash and delivery
  evidence, but it does not currently record the calling provider session. It
  is useful in a three-way join, not sufficient by itself for authorship.

## Trustworthy cross-log join

Classify a Submitted Web/TUI block as agent-authored only when all of these hold:

1. Parse a supported relay invocation from a provider transcript; do not search
   arbitrary shell output for similar prose.
2. Recover the exact submitted byte string after shell/JSON decoding.
3. Require a successful delivery result from the same tool-call identity.
   A command without its result, a failed result, or an ambiguous transport
   outcome is not evidence of delivery.
4. Bind the delivery destination to the target Orc coordinator/session. A
   matching message delivered to another team is not a match.
5. Match the submitted UTF-8 bytes exactly to one Orc content-block occurrence.
6. Require source delivery to precede Orc ingestion. Use elapsed time only as a
   sanity bound; queued delivery means nearest-timestamp matching is unsafe.
7. Require the candidate to be unique. Multiple successful candidates with the
   same bytes and plausible destination/time remain ambiguous unless an API
   acknowledgement ID dereferences the exact Orc occurrence.

The resulting attribution should retain evidence fields: source provider,
source session ID, source tool-call ID, delivery-result ID, destination,
content SHA-256, source/ingest timestamps, lag, and confidence basis
`exact-successful-cross-log-delivery`.

## False-positive controls

- Never infer owner authorship from `role=user`, Web/TUI ingress, first-person
  prose, a browser view, or a prompt-like shape.
- Never infer agent authorship solely from prefixes such as `FROM`, `RELAY`, or
  `[agent]`; those are useful search hints only.
- Do not fuzzy-match normalized Markdown. Exact bytes prevent two similar
  status reports from collapsing into one attribution.
- Do not use `v2-message-N` alone; use the unique block occurrence.
- Do not select the closest timestamp when more than one candidate survives.
- Do not promote a self-reported short name to a durable agent identity.
- Preserve contradictory evidence and classify the occurrence as ambiguous.

An operator-supplied project rule is a separate evidence class from a cross-log
join. It may declare that a named ingress was exclusively owner- or agent-used
over an exact half-open interval, with a durable reason and rule ID. Such a rule
must preserve the original unknown source label, reject overlaps, and never
claim to identify the individual sending agent. Exact delivery joins are
stronger and should supersede broad interval assertions as they become
available.

## Staged implementation

1. **Fail closed now.** Keep the existing intrinsic Orc/GChat classifications;
   classify Submitted Web/TUI input without intrinsic evidence or an explicit
   audited project rule as unknown or external/unknown.
2. **Index delivery evidence.** Mechanically extract successful known-relay
   calls and results from Codex and Claude transcripts into a content-addressed
   delivery table. This step is token-free.
3. **Join conservatively.** Apply the contract above and store provenance on
   the extracted occurrence without rewriting immutable raw data. Expose the
   evidence basis in the CLI/UI.
4. **Improve future capture.** Have relay tools send or log a generated delivery
   ID plus caller provider/session identity, and have Orc preserve that ID in
   `user_source` or `extra`. Then authorship becomes an intrinsic ID join rather
   than a content/time join.
5. **Backfill separately.** Re-run the cross-log linker as a versioned mechanical
   projection. New provider logs can improve old unknown classifications while
   unmatched history continues to degrade honestly.
