# Historical lifecycle marker fixtures

These payloads preserve the exact top-level fields written by released Python
`wrkslots` versions. The suffix names the revision that introduced that shape:

- `45b24f8` introduced `reclaim-started` and `recovery-started`.
- `e5074d1` added `runner` and `handoff_writer` to `reclaim-started`.
- `ded39ae` added those two fields to `recovery-started`.
- `2a61b65` added `salvage_archive_root` to `reclaim-started`.

The identity and timing values are inert fixture values; their keys and JSON
types match the values serialized by the historical writers.
