/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! This module just aggregates submodules.

mod files;
pub mod helpers;

/// Re-exported so `procfs` can report the SAME ceiling that `F_SETPIPE_SZ`
/// enforces. Two constants that must agree are one constant.
pub(crate) use files::DETERMINISTIC_PIPE_CAPACITY_BYTES;
mod io;
mod memory;
mod misc;
mod namespace;
pub(crate) mod robust_list;
mod signal;
pub(crate) mod socket_timestamp_ioctl;
mod sysinfo;
mod threads;
pub(crate) mod time;

use crate::consts::DET_SPECIAL_INODE_OFFSET;
use crate::resources::Device;
use crate::resources::ResourceID;
use crate::types::DetInode;
use crate::types::RawFd;

/// Give inherited standard streams identities that do not depend on backend
/// loader activity observed before the guest reaches its entry point. See
/// [`deterministic_stdio_inode_for_resource`] for which routes report them.
fn deterministic_stdio_inode(fd: RawFd) -> Option<DetInode> {
    (libc::STDIN_FILENO..=libc::STDERR_FILENO)
        .contains(&fd)
        .then_some(DetInode::mint(
            DET_SPECIAL_INODE_OFFSET.as_raw() + fd as u64,
        ))
}

/// The fixed inode that descriptor `fd` reports when it derives from the
/// guest's inherited standard streams, that is, when it carries the resource
/// `ContainerStdin`, `ContainerStdout` or `ContainerStderr`.
///
/// Those descriptors are fds 0 to 2 as hermit handed them over, their
/// duplicates, and descriptors opened through a link to one of them
/// (/dev/stdout, /proc/self/fd/1). Their object is wherever hermit's caller
/// pointed stdio (a terminal, a file, a pipe, /dev/null). Resolving it through
/// the inode pool would mint an inode on that object's device, and with one
/// counter per device (https://github.com/rrnewton/hermit/issues/2897) shift
/// every later inode there, so unrelated inode numbers would depend on how
/// hermit was invoked.
///
/// Fds 0 to 2 keep their numeric slot's inode, whichever stream they carry
/// (fd 2 after `2>&1` still reports 1002). A descriptor above fd 2 reports its
/// stream's inode. An ordinary file or pipe that has replaced fd 0, 1 or 2
/// carries its own resource and gets `None`.
///
/// The object's own path (/dev/null, or the file hermit's stdout was
/// redirected to) is not a descriptor route: it resolves through the pool, so
/// it gets the same inode whether or not the object is also hermit's stdio.
pub(crate) fn deterministic_stdio_inode_for_resource(
    fd: RawFd,
    resource: Option<ResourceID>,
) -> Option<DetInode> {
    let stream = match resource {
        Some(ResourceID::Device(Device::ContainerStdin)) => libc::STDIN_FILENO,
        Some(ResourceID::Device(Device::ContainerStdout)) => libc::STDOUT_FILENO,
        Some(ResourceID::Device(Device::ContainerStderr)) => libc::STDERR_FILENO,
        _ => return None,
    };
    deterministic_stdio_inode(fd).or_else(|| deterministic_stdio_inode(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_inode_namespace_is_fixed() {
        assert_eq!(
            deterministic_stdio_inode(libc::STDIN_FILENO),
            Some(DetInode::mint(1000))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDOUT_FILENO),
            Some(DetInode::mint(1001))
        );
        assert_eq!(
            deterministic_stdio_inode(libc::STDERR_FILENO),
            Some(DetInode::mint(1002))
        );
        assert_eq!(deterministic_stdio_inode(3), None);

        for resource in [
            ResourceID::Device(Device::ContainerStdin),
            ResourceID::Device(Device::ContainerStdout),
            ResourceID::Device(Device::ContainerStderr),
        ] {
            for fd in 0..=2 {
                assert_eq!(
                    deterministic_stdio_inode_for_resource(fd, Some(resource.clone())),
                    Some(DetInode::mint(1000 + fd as u64))
                );
            }
        }
        // Above fd 2, a duplicate or a descriptor opened through /dev/stdout
        // reports its stream's inode.
        for (stream, resource) in [
            ResourceID::Device(Device::ContainerStdin),
            ResourceID::Device(Device::ContainerStdout),
            ResourceID::Device(Device::ContainerStderr),
        ]
        .into_iter()
        .enumerate()
        {
            for fd in [3, 7, 1024] {
                assert_eq!(
                    deterministic_stdio_inode_for_resource(fd, Some(resource.clone())),
                    Some(DetInode::mint(1000 + stream as u64))
                );
            }
        }
        for fd in [3, 7] {
            assert_eq!(deterministic_stdio_inode_for_resource(fd, None), None);
            assert_eq!(
                deterministic_stdio_inode_for_resource(
                    fd,
                    Some(ResourceID::FileContents(DetInode::mint(1001))),
                ),
                None
            );
        }
        for fd in 0..=2 {
            assert_eq!(deterministic_stdio_inode_for_resource(fd, None), None);
            assert_eq!(
                deterministic_stdio_inode_for_resource(
                    fd,
                    Some(ResourceID::FileContents(DetInode::mint(1000))),
                ),
                None
            );
        }
    }
}
