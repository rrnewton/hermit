# Retired glossary migration audit

## Finding

The schema-3 glossary pipeline conflated two different questions:

1. A regular expression selected backticked strings, hyphenated tokens, and acronyms.
2. A model decided whether it could explain each selected string from bounded occurrences.

A `supported` definition therefore established explainability, not membership in a durable project
ontology. Transcript wrapper fields such as `command-args`, command examples, durations, commit
hashes, generic workflow adjectives, and even ordinary conjunctions could survive. The browser then
treated every projected entry as a site-wide link target. No threshold over occurrence count or
definition status repairs that missing semantic classification.

## Current compatibility policy

The paid schema-3 projection and its cache/usage receipts remain immutable. New summarize runs do
not generate glossary-definition jobs, and builds publish none of the legacy entries. A zero-token
rebuild is the migration for derived websites: it removes stale glossary arrays and generated
catalog links without deleting the historical evidence.

`wrkviz audit-glossary --output ARCHIVE [--details]` is a read-only diagnostic. It
mechanically separates definite noise from plausible projects, systems, workstreams/tasks, and
milestones that merit a future semantic pass. Both dispositions remain unpublishable. The report is
stable JSON with `--format json`; it acquires no archive lock, writes no file, and calls no model.

This conservative boundary means useful old strings such as `e9patch`, `KVM`, or a named Reverie pin
update may appear in `semantic-review-required` but do not become links merely because a heuristic
noticed words such as “backend” or “workstream” in their cached definitions. That is graceful
degradation, not data loss: the source occurrences, paid definition, and receipt remain available.

## Required next semantic contract

A model-backed replacement should be a separately registered summarizer rather than a new filter on
schema 3. Its output needs:

- a closed kind such as project, subsystem/system, workstream/task, or milestone;
- one canonical display name plus evidence-backed aliases;
- a newcomer definition grounded only in retained occurrences and the project overview;
- exact occurrence/event provenance and chronological availability;
- explicit rejection of commands, fields, raw identifiers, process verbs, and generic language;
- stable identity and a knowledge-epoch/content hash so upgrades are incremental and auditable.

Only that versioned semantic projection should feed terminology consistency prompts and browser
linkification. Reusing schema-3 `supported` as publication authority would recreate the original
bug.
