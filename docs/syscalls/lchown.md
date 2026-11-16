---
schema: syscall-pages/v2
syscall: lchown
nr_x86_64: 94
related:
  - https://github.com/rrnewton/hermit/issues/629
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lchown&body=Discussion+for+the+%60lchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lchown%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_ownership_change_noop_syscall"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `lchown` (94) as determinized via pred:is_ownership_change_noop_syscall dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Classification: `detcore/src/syscall_classification.rs:1692`
- Dispatch: `detcore/src/lib.rs:7634`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `lchown`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lchown&body=Discussion+for+the+%60lchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lchown%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · hermit 239ca43d3092 · facts-only page generated from Detcore code facts at this revision.
