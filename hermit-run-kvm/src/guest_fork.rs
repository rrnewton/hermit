/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): how a KVM run copies its guest for
//! multiverse timelines.
//!
//! The guest's VMs, memory and executor live in this process, so forking
//! this process (`detcore::timelines::fork_this_process`) copies the guest
//! with it: there is nothing more to fork. Each copy rebuilds what that fork
//! does not copy (`reverie_kvm::host_fork`), and its guest's descriptors are
//! host descriptors of its own process
//! (`detcore::timelines::own_process_view`).

use detcore::timelines::ForkFuture;
use detcore::timelines::ForkSide;
use detcore::timelines::GuestFork;
use detcore::timelines::GuestHolder;
use detcore::timelines::HostView;
use detcore::timelines::MemoryCopy;

/// The KVM backend's guest fork.
pub struct KvmGuestFork;

/// The one instance, for `detcore::timelines::install_timelines`.
pub static KVM_GUEST_FORK: KvmGuestFork = KvmGuestFork;

impl GuestFork for KvmGuestFork {
    fn name(&self) -> &'static str {
        "kvm"
    }

    fn hold(&self) -> Result<String, String> {
        reverie_kvm::host_fork::prepare().map(|report| format!("{report:?}"))
    }

    fn fork_guest<'a>(
        &'a self,
        _holder: Option<&'a mut dyn GuestHolder>,
    ) -> ForkFuture<'a, Result<ForkSide, String>> {
        // The guest lives in this process: forking it forks the guest.
        Box::pin(std::future::ready(detcore::timelines::fork_this_process()))
    }

    fn materialize(&self, memory: Option<MemoryCopy>) -> Result<String, String> {
        if let Some(memory) = memory {
            reverie_kvm::host_fork::set_private_arenas(memory == MemoryCopy::Lazy);
        }
        reverie_kvm::host_fork::repair_in_child().map(|report| format!("{report:?}"))
    }

    fn exit_report(&self) -> String {
        reverie_kvm::host_fork::exit_report()
    }

    fn host_view(&self) -> &'static dyn HostView {
        detcore::timelines::own_process_view()
    }
}
