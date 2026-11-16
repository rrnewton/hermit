---
schema: syscall-pages/v2
syscall: request_key
nr_x86_64: 249
related:
  - https://github.com/rrnewton/hermit/issues/403
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+request_key&body=Discussion+for+the+%60request_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frequest_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60request_key%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_kernel_keyring_syscall"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
---

## Strategy

Detcore classifies `request_key` (249) as determinized-refused via pred:is_kernel_keyring_syscall dispatch; deterministic refusal: true; run subscription: true; record/replay subscription: true.

- Classification: `detcore/src/syscall_classification.rs:1791`
- Dispatch: `detcore/src/lib.rs:7415`

This facts-only page was generated from Detcore code facts. Checked analysis prose has not been added yet.

## Discussion

[Start a discussion about `request_key`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+request_key&body=Discussion+for+the+%60request_key%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frequest_key.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60request_key%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Status

- 2026-11-15 · hermit 239ca43d3092 · facts-only page generated from Detcore code facts at this revision.
