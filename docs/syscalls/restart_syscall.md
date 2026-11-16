---
schema: syscall-pages/v2
syscall: restart_syscall
nr_x86_64: 219
related:
  - https://github.com/rrnewton/hermit/issues/404
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: unsupported
  dispatch: "unsupported"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `restart_syscall` (219) as unsupported via unsupported dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Classification: `detcore/src/syscall_classification.rs:2016`
- Dispatch: `detcore/src/lib.rs:8476`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `restart_syscall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+restart_syscall&body=Discussion+for+the+%60restart_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frestart_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60restart_syscall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · hermit 239ca43d3092 · facts-only page generated from Detcore code facts at this revision.
