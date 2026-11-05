# Summary backend comparison experiments

> **Provenance.** This is a dated investigation record. It was produced against a private
> downstream workspace, so it refers to repositories, hosts and services outside this one
> that cannot be resolved from here. Projects that consume `agent-utils` appear under
> neutral labels — `consumer-a`, `consumer-b` — and those are REDACTIONS, not real names:
> `agent-utils` is a reusable library and has to read standalone, without naming whoever
> happens to use it. **The measurements, dates and findings are unchanged**; only names
> were replaced. Nothing here describes `agent-utils` itself.
> See `#86 scrub-client-names` and `#67 standalone-repo`.

## 2026-08-05 controlled experiment

### Bottom line

This was **not a completed three-LLM comparison**. It was one controlled
comparison between the deterministic heuristic and Sol, plus an attempted Luna
run that never reached generation:

- The controlled heuristic and Sol runs used the same frozen input, pipeline
  revision, job partitioning, prompt version, batching, and reasoning setting.
  They are useful for comparing the free deterministic fallback with Sol's
  output, but the heuristic is code, not an LLM.
- Luna produced no summaries. The requested `gpt-5.6-luna` deployment was not
  available to the backend. Its eight failed receipts contain `usage: null`, so
  there is **no Luna token cost to report**; null usage is not evidence of zero
  spend.
- The Orc total of **2,704,708 successful tokens** (or **2,774,129 all-in**) is
  a different workload, not another model result. It must not be placed in a
  model-comparison cost chart beside the Codex/Consumer A run.

The exact controlled inputs and results are retained under `~/temp/` for
inspection. The archive format and normal workflow are described in the
[project README](../README.md).

### Controlled workload

The immutable source archive was
`~/temp/codex-consumer-a-timeline-benchmark-source`. Its digest was:

```text
bcaa65af434f9d2bff148a6b6a92b46336e2465ca55f7ef696575cb36ee34b20
```

All three attempted modes ingested that digest. The snapshot contained:

| Measure | Count |
|---|---:|
| Source transcript files / agent tracks | 51 |
| Events | 4,181 |
| Outer tool calls | 12,920 |
| Imported interaction edges | 1,207 |
| Source bytes | 238,738,644 |

The summarizer produced the same 228 logical jobs for the controlled heuristic
and Sol runs: 172 work phases, 51 hindsight agent names, and five calendar
rollups. Both used the `wrkviz-summary-v1` and
`wrkviz-agent-name-v1` prompt versions at revision `f59efe6`, with:

| Control | Value |
|---|---:|
| Display timezone | `America/New_York` |
| Phase width | 30 minutes |
| Prior-context limit | 16,000 characters |
| Transcript limit | 30,000 characters |
| Summary batch size | 6 jobs |
| Name batch size | 12 agents |
| Workers | 3 |
| Reasoning effort | `medium` |

These settings yield 39 successful backend batches when starting from an empty
cache: 29 phase batches, five naming batches, and five ordered rollup batches.
The heuristic control was generated from revision `f59efe6`; the Sol benchmark
was run from the same working revision. The run metadata does not itself record
the Git SHA, so the latter is operator provenance rather than a property that
can be re-derived from the archive. Future experiments should persist the tool
SHA.

### Results

`total` is `input + output`. Cached input is already included in input, and
reasoning output is already included in output; neither should be added to the
total a second time.

| Attempt | Artifact directory |
|---|---|
| Deterministic heuristic | `~/temp/07_codex-consumer-a-controlled-heuristic` |
| Luna (incomplete) | `~/temp/codex-consumer-a-timeline-luna` |
| Sol control | `~/temp/04_codex-consumer-a-controlled-sol` |

| Attempt | Completed receipts | Failed receipts | Successful artifact total | Failed-call overhead | All recorded spend | Result |
|---|---:|---:|---:|---:|---:|---|
| Deterministic heuristic | 39 | 0 | 0 | 0 | 0 | Complete; not an LLM |
| `gpt-5.6-luna` | 0 | 8 | unknown | unknown | unknown | No summaries; deployment unavailable |
| `gpt-5.6-sol` | 39 | 6 | 2,253,434 | 396,252 | 2,649,686 | Complete |

The Luna row must not be rendered as zero tokens. Two top-level attempts
created eight failed batch receipts, all with null usage. The persisted error is
a backend exit before a usable response; the deployment diagnosis during the
experiment was HTTP 404/unavailable. There is neither a summary artifact nor a
metered token record to compare with Sol.

#### Sol token detail

| Sol receipt set | Input | Cached input (subset) | Output | Reasoning output (subset) | Total |
|---|---:|---:|---:|---:|---:|
| 39 completed artifact receipts | 2,167,643 | 170,752 | 85,791 | 16,491 | 2,253,434 |
| 6 failed validation receipts | 380,405 | 29,696 | 15,847 | 1,386 | 396,252 |
| All recorded Sol calls | 2,548,048 | 200,448 | 101,638 | 17,877 | 2,649,686 |

Sol needed five failed top-level summarize attempts before a complete run. Six
individual calls returned token-bearing output that failed structural
validation, chiefly because work-summary bullets were not chronological or a
bullet timestamp fell outside its interval. Successful batches were cached
between attempts. Thus 2,253,434 is the generation cost behind the final
controlled artifacts, while 2,649,686 is the actual all-in recorded spend for
the experiment. The difference is real retry overhead, not cache replay.

The immediate replay was idempotent: 228 cache hits, zero misses, zero backend
batches, zero newly spent tokens, and a subsequent build changed zero files.

### Plain-language feature extension

`~/temp/05_codex-consumer-a-sol-feature-complete` was copied from the controlled
Sol output and then updated by a later pipeline revision. It uses the same
source digest and model, but it is **not another model arm** and is not strictly
comparable to the controlled run: the technical rollup prompt changed and five
new plain-language rollups were added, giving ten changed/new jobs.

| Quantity | Tokens |
|---|---:|
| New successful spend for the feature migration | 242,331 |
| Token cost behind the final feature-complete artifact set | 2,375,037 |
| Superseded successful receipts retained for audit | 120,728 |
| All successful receipts accumulated in the directory | 2,495,765 |
| Inherited failed-call overhead | 396,252 |
| Cumulative all-in recorded spend | 2,892,017 |

Its replay had 233 cache hits, zero misses, zero backend batches, zero newly
spent tokens, and a zero-change build. Those numbers demonstrate idempotence;
they do not turn the feature migration into an apples-to-apples model test.

### Qualitative observations

The heuristic and Sol artifacts can be compared on identical transcript data,
but this inspection was not blinded or scored. One daily-summary comparison
illustrates the broad difference:

- The deterministic heuristic repeated the same sentence across multiple time
  buckets, exposed a raw timestamp and `ASSISTANT:` excerpt, and ended several
  items with mechanical truncation.
- Sol synthesized a coherent outcome, preserved concrete counts and findings,
  and produced a chronological work summary rather than copying nearby text.
- Sol's six validation failures show that better prose did not guarantee schema
  discipline; strict validation and durable failed-call accounting materially
  affected its real cost.
- Luna has no output and therefore no quality observation.

The later plain-language Sol sample is easier to approach than the technical
rollup, but it still contains project terms such as `AGENTS.md`, registry, and
skill adapter. That is a prompt/product observation, not evidence of model
superiority, because no other model produced the same plain-language jobs.

### Why the Orc number is separate

The Orc artifact is `~/temp/06_orc-consumer-a-day1-sol`. It summarizes a
different provider snapshot and date window with different transcript density,
lineage, prompts, and pipeline features. Its imported backing data contained
1,413 agent incarnations, 42,761 events, and 24,593 outer tool calls; the
selected day produced 219 phases, 18 displayed agents, and both technical and
plain-language rollups: 245 logical jobs in all. Its source digest was
`977e3d84f3a3c8c692485880ecf7f7494ccbc99386d1a8d2cfdf864ffe35e27c`.

| Orc Sol receipt set | Input | Cached input (subset) | Output | Reasoning output (subset) | Total |
|---|---:|---:|---:|---:|---:|
| 90 completed receipts | 2,608,801 | 326,656 | 95,907 | 12,596 | 2,704,708 |
| 1 failed receipt | 67,075 | 7,424 | 2,346 | 108 | 69,421 |
| All recorded calls | 2,675,876 | 334,080 | 98,253 | 12,704 | 2,774,129 |

Therefore, 2,704,708 is the cost behind successful Orc artifacts and 2,774,129
is its all-in cost. Neither number says whether Sol is cheaper or more expensive
than another model: there is no second model on the same Orc input, and this is
not the controlled Codex/Consumer A workload.

### Evidence and audit method

The figures above were recomputed from JSON rather than copied from console
output:

- `manifest.json` supplies each source digest.
- `runs/*.json` supplies commands, job/cache counts, and replay results.
- `teams/<team>/summary_data/{cache,name_cache}/_usage/receipts/*.json`
  supplies per-call status and token counters.
- The successful artifact total in a completed summarize run is the deduplicated
  sum of receipts referenced by the returned artifacts. Summing every receipt
  instead gives historical spend, including failed and superseded calls.

All receipt totals were checked as `total_tokens == input_tokens +
output_tokens`. Cache-write input was zero in every receipt discussed here.
No dollar estimate is given because the artifacts do not carry a dated,
model-specific price schedule.

### Requirements for a valid three-model comparison

1. Pin and record one Git revision, prompt versions, source digest, timezone,
   date bounds, phase/context/transcript limits, batch sizes, worker count, and
   reasoning setting.
2. Select three actual LLM deployment identifiers and make a tiny availability
   probe for each before starting the full run. An unavailable arm remains a
   failed arm; do not silently substitute a model.
3. Start each arm from a fresh output directory containing the same immutable
   source snapshot and no summary cache. Do not compare against a live or later
   snapshot.
4. Apply the same retry cap and validation policy. Report both the deduplicated
   successful-artifact cost and all-in cost, including failed calls.
5. Replay every completed arm and require zero cache misses, zero backend calls,
   and zero changed build files.
6. Blind the model labels and score the same sampled phases, names, and rollups
   for factual accuracy, omitted work, chronology, terminology consistency,
   plain-language accessibility, opaque references, and unsupported claims.
7. Record wall time separately from tokens. Convert tokens to money only with a
   dated price table that distinguishes cached input, uncached input, and
   output for each model.

Until those steps produce three completed LLM arms, describe this evidence as a
heuristic-versus-Sol control with a failed Luna availability attempt—not a
three-model benchmark.

## 2026-08-06 live-session experiment

This follow-up froze a much larger, newer snapshot of the same Codex coordinator
lineage. It produced two complete websites: `gpt-5.6-sol` and `gpt-5.5` using
the explicit `priority` service tier. Terra and Luna remained unavailable at
the provider, while GPT-5.4-mini was retired or unroutable. The two completed
outputs support same-snapshot inspection, but not a controlled throughput
claim: the legacy Sol receipts did not record their service tier, and the
GPT-5.5 run changed phase batch size during recovery.

### Frozen workload and output directories

Every arm copied and ingested the same source snapshot:

| Measure | Value |
|---|---:|
| Source digest | `934bda3c981c1af0151673bac7327ca0a7b4a3e4b42224089db9ff1bfc85a3dd` |
| Source transcript files / agents | 142 |
| Source bytes | 653,329,377 |
| Source lines | 389,732 |
| Events | 8,040 |
| Outer tool calls | 25,787 |
| Imported interaction edges | 2,316 |
| Extracted artifacts | 1,313 |
| Repositories | 8 |
| Logical summarization jobs | 577 |
| Latest imported event | `2026-08-06T12:03:07.253Z` (`2026-08-06 08:03:07.253 EDT`) |

The root session was `019fcfe7-0f68-7301-8aab-c2f90a7026c7`. The display
timezone was `America/New_York`; the declared primary project and host were
`dev-consumer-a` and `devbig014`.

| Attempt | Artifact directory |
|---|---|
| `gpt-5.6-sol` | `~/temp/codex-coord-latest-sol` |
| `gpt-5.6-terra` | `~/temp/codex-coord-latest-terra` |
| `gpt-5.6-luna` | `~/temp/codex-coord-latest-luna` |
| `gpt-5.4-mini` availability probe | `~/temp/codex-coord-latest-5.4-mini` |
| `gpt-5.5`, `priority` tier | `~/temp/codex-coord-latest-gpt-5.5-fast` |

### Results and deployment availability

| Arm | Service tier | Completed receipts | Failed receipts | Result | Actual experiment spend |
|---|---|---:|---:|---|---:|
| Sol | Unrecorded legacy receipts | 96 | 3 | Complete website | 5,423,037 known tokens |
| Terra | Unrecorded legacy; latest probe `default` | 0 | 14 | No summaries; deployment missing | unknown |
| Luna | Unrecorded legacy; latest probe `default` | 0 | 30 | No summaries; deployment missing | unknown |
| GPT-5.4-mini | Unrecorded legacy receipts | 0 | 8 | No summaries; HTTP 421, retired/unroutable | unknown |
| GPT-5.5 | `priority` | 153 | 4 | Complete website | 6,142,518 known tokens |

Terra and Luna were visible and marked API-supported in the model catalog, but
their live Azure transport traces returned HTTP 404 `DeploymentNotFound`.
Terra's archive retains that full text. Luna's archive retains only the
truncated endpoint/error tail; its full status was observed in the temporary
transport trace during the run and is not independently recoverable from the
archive alone. Luna's error included Azure's generic suggestion to wait five
minutes; the observed HTTP status and error code distinguish this from a
rolling token quota. GPT-5.4-mini returned “no upstream configured for this
host,” and its catalog entry is hidden/retired with an upgrade recommendation
to Luna.

Every Terra, Luna, and GPT-5.4-mini receipt has `usage: null`; their spend is
unknown, not zero. Fresh explicit-default probes added two failed receipts
apiece to Terra and Luna without creating cache artifacts. The probe records
are `runs/20260806T142003303321Z-878de678.json` for Terra and
`runs/20260806T142045939904Z-16e122e5.json` for Luna. GPT-5.5's four
validation-failed calls do contain usage and are included in its all-in total.
Only an all-cache-hit replay that makes no backend calls is a known zero-token
run.

Luna is the requested default for the next comparison once its deployment is
available. That preference does not turn these failed probes into a result:
the prior Luna attempts produced no summaries, and their null usage means the
tokens spent, if any, remain unknown rather than zero.

There is no Spark model identifier, including no `gpt-5.3-spark`, in the
current nine-model catalog. “Fast” is instead the `priority` service tier. The
GPT-5.5 archive records that tier in every v2 receipt, run report, and cache
identity. Sol predates this provenance, so its service tier remains unknown.

### Sol cost, completion, and idempotence

The complete Aug. 6 Sol rebuild spent 5,423,037 tokens across all attempts:

| Receipt set | Input | Output | Total |
|---|---:|---:|---:|
| 96 completed receipts | 5,007,735 | 198,405 | 5,206,140 |
| 3 failed receipts | 210,123 | 6,774 | 216,897 |
| All Aug. 6 attempts | 5,217,858 | 205,179 | 5,423,037 |

The successful final invocation reused 511 cached jobs and generated the
remaining 66 jobs in 21 backend batches. It took about 4m36s and newly spent
636,615 tokens: 617,311 input tokens (including 155,904 cached-input tokens)
and 19,304 output tokens (including 3,699 reasoning tokens).

From ingestion start through the first complete website build, the experiment
took about 24m42s. Including the all-hit replay and zero-change rebuild, it took
about 25m12s. The resulting site contains 142 agents, 391 phases, 2,811 edges,
33,041 rendered events, six calendar rollups, and 547 Markdown summary files.

The idempotence replay recorded 577 cache hits, zero misses, zero backend
batches, and zero newly spent tokens. The following build changed zero files.
The successful completion record is
`runs/20260806T122734995728Z-d8da957c.json`.

Three Sol totals answer different questions and must not be added together:

- **5,423,037 tokens** is the actual Aug. 6 ledger increase, including failed
  attempts.
- **5,293,891 tokens** is generation provenance attached to the returned
  artifact set. It includes 87,751 tokens of reused Aug. 5 artifacts and is not
  additional spend by the final invocation.
- **8,315,054 tokens** is the output directory's lifetime ledger, including
  2,892,017 tokens from the earlier experiment and feature-development history.

The current-run result is independently recoverable as
`8,315,054 lifetime - 2,892,017 inherited = 5,423,037`.

### GPT-5.5 priority cost, recovery, and idempotence

The complete GPT-5.5 priority arm spent 6,142,518 tokens across all attempts:

| Receipt set | Input | Cached input (subset) | Output | Reasoning output (subset) | Total |
|---|---:|---:|---:|---:|---:|
| 153 completed artifact receipts | 5,587,005 | 788,480 | 323,514 | 100,145 | 5,910,519 |
| 4 failed receipts | 217,810 | 22,528 | 14,189 | 2,800 | 231,999 |
| All recorded calls | 5,804,815 | 811,008 | 337,703 | 102,945 | 6,142,518 |

The first three top-level attempts used phase batches of six. They retained 84
valid jobs, but four calls failed structural validation, chiefly because a work
summary timestamp fell outside its phase. Reducing only the phase batch size to
three completed the cache. That successful invocation generated 493 remaining
jobs in 139 backend batches over 33m20s and newly spent 5,096,871 tokens. The
5,910,519-token artifact provenance includes the valid batches retained from
the earlier attempts; the 231,999-token difference to the all-in ledger is
failed-call overhead.

From the first model attempt through complete summaries took about 39m41s; the
first website was ready after about 43m08s. Including the all-hit replay and
zero-change rebuild took about 45m50s. The resulting site has the same 142
agents, 391 phases, 2,811 edges, and 547 Markdown summary files as Sol.

The widest recorded wall-clock envelope, from ingest start through the final
zero-change build, was about 1h26m01s. It includes an intentional 38m14s pause
between ingest and the first model attempt plus later inter-command gaps; it is
therefore an upper bound on pipeline time, not model latency. Summing the eight
recorded command durations gives about 43m40s of active command time.

The replay restored the original batch-size-six command and recorded 577 cache
hits, zero misses, zero backend calls, and zero newly spent tokens. Its following
build changed zero files. The successful run, replay, and final build are:

- `runs/20260806T145114506396Z-e4021a7b.json`
- `runs/20260806T145607809173Z-33c36a9a.json`
- `runs/20260806T145723511951Z-50415762.json`

### Interpretation

This experiment produced two complete same-snapshot LLM outputs, but it is not
a controlled speed benchmark. GPT-5.5 explicitly used `priority`; Sol's legacy
service tier is unrecorded. GPT-5.5 also needed three failed batch-size-six
attempts before batch size three completed successfully. Those retries and
smaller batches materially affect elapsed time, and the observed Fast arm did
not finish faster than Sol.

The experiment does validate frozen-source parity, service-tier provenance for
new receipts, durable partial caches, exact all-in accounting, and idempotent
replay. Terra and Luna still have no summaries, so there is no four-model
quality or cost result. Luna remains the requested future default arm, but it
must first pass an availability probe with durable usage accounting. Some Sol
run commands and receipt paths also retain pre-rename `~/temp/` names even
though the current documented directory exists. A rigorous head-to-head still
requires available deployments, identical recorded tiers and batching, empty
caches, identical retry policy, and blinded quality scoring.
