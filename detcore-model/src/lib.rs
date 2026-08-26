/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely-shared type definitions.

/// Exit status for a run HERMIT DELIBERATELY REFUSED, as distinct from one
/// where hermit itself broke.
///
/// ⚠️ "REFUSED" AND "BROKE" DEMAND OPPOSITE RESPONSES, AND THEY WERE THE SAME
/// NUMBER. A fail-closed policy stopping a run is hermit working correctly: the
/// operator must read the refusal and change their program or their flags.
/// `HERMIT_INTERNAL_FAILURE_EXIT` (125) says hermit is broken and the operator
/// must file a bug. Reporting the first as the second sends a reader looking
/// for a defect in a shutdown path that behaved exactly as designed.
///
/// ⚠️ WHY IT IS NOT 1, WHICH IS WHAT THIS PATH USED TO EXIT.
/// `unrecoverable_shutdown` called `std::process::exit(1)`, so the container
/// child exited 1 and the parent's classifier — whose arm is documented "the
/// child died with a status it did not pick" — turned a status hermit HAD
/// picked into `class=container-child-exit` and 125. Restoring 1 would fix the
/// misclassification and reintroduce a worse one: 1 is the commonest guest exit
/// status, so it cannot distinguish "hermit refused" from "your program
/// returned 1".
///
/// ⚠️ WHY 122. It sits immediately below the reserved band (123 safehermit log
/// cap, 124 deadline, 125 hermit broke, 126/127 GNU exec-level), keeping the
/// reserved codes contiguous, and it is the cheapest possible narrowing of the
/// guest range. See the full allocation above the constants in
/// `hermit-cli/src/lib.rs`.
///
/// ⚠️ IT LIVES IN `detcore-model` BECAUSE BOTH SIDES NEED IT. `detcore` emits it
/// and `hermit-cli` recognises it; `detcore-model` is the only crate both
/// depend on. A copy on each side is exactly the defect that left eight cli
/// tests asserting a stale exit status for a day after the product moved.
pub const HERMIT_POLICY_REFUSAL_EXIT: i32 = 122;

// ⚠️ THE VALUE IS PINNED, NOT ONLY NAMED. `tests/cli.rs` and the allocation
// table both assert 122; a one-character edit here would move every consumer
// with it and nothing would fail. 0 is called out separately because it is the
// dangerous drift: at 0 a refusal would report SUCCESS.
const _: () = assert!(
    HERMIT_POLICY_REFUSAL_EXIT == 122,
    "122 keeps the reserved band contiguous below 123/124/125/126/127"
);
const _: () = assert!(
    HERMIT_POLICY_REFUSAL_EXIT != 0,
    "a refusal exiting 0 would report success"
);

pub mod collections;
pub mod config;
pub mod fd;
pub mod futex;
pub mod happens_before;
pub mod pedigree;
pub mod pid;
pub mod schedule;
pub mod summary;
pub mod time;
