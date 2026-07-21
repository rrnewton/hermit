/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Serialize;

use crate::pid::DetTid;

/// Identity of a Linux memory address space (`mm_struct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MmId {
    creator: DetTid,
    generation: u64,
}

impl MmId {
    /// Create the initial address space owned by a task.
    pub const fn initial(creator: DetTid) -> Self {
        Self {
            creator,
            generation: 0,
        }
    }

    /// Inherit an address space for `CLONE_VM`, otherwise create a fresh identity.
    pub const fn for_clone(parent: Self, child: DetTid, shares_vm: bool) -> Self {
        if shares_vm {
            parent
        } else {
            Self::initial(child)
        }
    }

    /// Create the replacement address space installed by exec.
    pub fn for_exec(self, creator: DetTid) -> Self {
        let generation = if self.creator == creator {
            self.generation + 1
        } else {
            0
        };
        Self {
            creator,
            generation,
        }
    }
}

/// Identity of a supported private futex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FutexID {
    /// Address space containing the futex word.
    pub mm: MmId,
    /// Virtual address of the futex word within `mm`.
    pub address: usize,
}

impl FutexID {
    /// Create a private futex key.
    pub const fn private(mm: MmId, address: usize) -> Self {
        Self { mm, address }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_vm_controls_private_futex_aliasing() {
        let parent = MmId::initial(DetTid::from_raw(10));
        let child = DetTid::from_raw(11);

        assert_eq!(
            FutexID::private(parent, 0x1000),
            FutexID::private(MmId::for_clone(parent, child, true), 0x1000),
            "CLONE_VM tasks should use the same private futex key"
        );
        assert_ne!(
            FutexID::private(parent, 0x1000),
            FutexID::private(MmId::for_clone(parent, child, false), 0x1000),
            "a copied address space should not alias the parent's private futex"
        );
        assert_ne!(
            FutexID::private(parent, 0x1000),
            FutexID::private(parent.for_exec(DetTid::from_raw(10)), 0x1000),
            "exec should replace the private futex namespace"
        );
    }
}
