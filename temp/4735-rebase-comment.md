**Mechanical rebase onto fresh main — 4735**

Rebased `drain/4735` from `90bbe4baf480` onto fresh main `5bd5f765f5fe29a790e756999c755e4b4171243b`. New head: `c247c0e95c473f49cff73b983ecd3a620014076d` (pushed with lease vs `90bbe4baf480`, read back via ls-remote).

Range-diff vs the approved head: every surviving commit is `=` except explainable `!` for the `detcore-model/src/config.rs` legacy-field union (main's `property_snapshot` + 4735's `rewrite_exclusives_unmarked`/`overlay`, counts 54→55, offset 39→40), the `run.rs`/`tool_global.rs` unions (both sides kept), and the RPC renumber. Two pin-count commits became empty against main's DAG and were dropped; the DAG pair was regenerated from main and is canonical (`generate-validation-dag --check` passes, no pin drift).

RPC: main landed revision 41 (`HostFileIdentity::content`, PR 4791) after 4735 allocated 41, so 4735's `StoreExclusive Rewrite::unmarked` row renumbers 41→42 (reserves 33–39 untouched, payload types unchanged) with digest re-measured on the combined tree: `7ba6b2766bf2230a`. Guard test passes.

Targeted tests at `c247c0e9` (x86_64, no aarch64 execution claim): hermit-detcore `aarch64_lse` 77 pass, `record_serde` 10, `reserved_input` 64, `preemptions` 114, RPC guard 1; detcore-model `config` 109; hermit-run `provenance` 16, `aarch64_lse` 1; hermit-manifest-plan `validation_dag` pass (full package: 717 pass, 6 host runner-probe failures needing a built hermit binary — pre-existing environmental class); `cargo fmt --check` clean; clippy on touched crates clean.

Repo-cleanliness: clean, 26 files, product paths only, no binaries, no file over 2MiB.

Approvals carry only if this reads as mechanical; resubmitted to the merge queue for validation. Not landed.
