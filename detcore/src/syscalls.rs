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

use serde::Deserialize;
use serde::Serialize;

use crate::consts::DET_SPECIAL_INODE_OFFSET;
use crate::resources::Device;
use crate::resources::ResourceID;
use crate::types::DetInode;
use crate::types::RawDevice;
use crate::types::RawFd;
use crate::types::RawFileId;

/// The fixed inode of standard stream `stream`: 1000, 1001 or 1002 for stdin,
/// stdout and stderr. See [`InheritedStdio`] for which objects report it.
pub(crate) fn deterministic_stdio_inode(stream: RawFd) -> Option<DetInode> {
    (libc::STDIN_FILENO..=libc::STDERR_FILENO)
        .contains(&stream)
        .then_some(DetInode::mint(
            DET_SPECIAL_INODE_OFFSET.as_raw() + stream as u64,
        ))
}

/// The standard stream a descriptor derives from, decided by its resource and
/// not by its number: `ContainerStdin`, `ContainerStdout` or `ContainerStderr`
/// marks fds 0 to 2 as hermit handed them over, their duplicates wherever they
/// land (fd 2 after `2>&1` carries stdout's resource), and descriptors opened
/// through a link to one of them (/dev/stdout, /proc/self/fd/1). An ordinary
/// file or pipe that has replaced fd 0, 1 or 2 carries its own resource and
/// gets `None`.
pub(crate) fn inherited_stdio_stream(resource: Option<ResourceID>) -> Option<RawFd> {
    match resource {
        Some(ResourceID::Device(Device::ContainerStdin)) => Some(libc::STDIN_FILENO),
        Some(ResourceID::Device(Device::ContainerStdout)) => Some(libc::STDOUT_FILENO),
        Some(ResourceID::Device(Device::ContainerStderr)) => Some(libc::STDERR_FILENO),
        _ => None,
    }
}

/// The host identities, `(st_dev, st_ino)`, of the objects hermit handed the
/// guest as fds 0, 1 and 2, indexed by stream. Recorded once for the root
/// process and inherited unchanged by every process of the run, so a guest
/// that closes or replaces its own fd 1 does not change what "hermit's stdout"
/// is.
///
/// Those objects are wherever hermit's caller pointed stdio: a terminal, a
/// file, a pipe, /dev/null. Each has ONE deterministic identity on every
/// route that reaches it, the fixed inode `1000 + s` of the lowest stream `s`
/// it was handed on: a descriptor derived from a stream, a link to such a
/// descriptor, the object's own path, a directory entry, fdinfo, a
/// `/proc/<pid>/maps` header, and a `pipe:[N]` or `socket:[N]` link. Two
/// identities for one object break programs that compare them: with hermit's
/// stdout appended to `F`, `cat F` used to copy `F` onto itself without end
/// and `cp F /dev/stdout` truncated `F` before reading it, because `fstat(1)`
/// and `stat("F")` disagreed. An object handed on several streams (a
/// terminal on all three, or `> F 2>&1`) reports the lower stream's inode on
/// all of them, as the streams share one inode on Linux. So which streams
/// report equal inodes depends on how hermit was invoked, exactly as it does
/// natively; no other inode does.
///
/// Whether a route consults the inode pool is decided by the ROUTE, not by
/// the object. A descriptor derived from a stream, and a descriptor link into
/// another process's table that reaches a stdio object, never do: the object
/// behind them varies with the invocation, and with one counter per device
/// (https://github.com/rrnewton/hermit/issues/2897) a mint for it would shift
/// every later inode on its device. The object's own path, a directory
/// listing, and a descriptor opened through that path still resolve through
/// the pool and discard the result, so they use up the same pool slot whether
/// or not the object is also hermit's stdio. Neither the reported inode nor
/// the pool's numbering of any other file therefore depends on where hermit's
/// stdio points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedStdio {
    /// `(st_dev, st_ino)` of each stream's object.
    objects: [Option<RawFileId>; 3],
    /// The `(device, inode)` a `/proc/<pid>/maps` header gives a mapping of
    /// each stream's object, when it was learnt at startup (see
    /// [`InheritedStdio::inode_of_mapping`]).
    mapped: [Option<RawFileId>; 3],
}

impl InheritedStdio {
    /// The identities of stdin, stdout and stderr, `None` for a stream hermit
    /// was started without.
    pub(crate) const fn new(streams: [Option<RawFileId>; 3]) -> Self {
        InheritedStdio {
            objects: streams,
            mapped: [None; 3],
        }
    }

    /// These objects, with the identity a maps header gives a mapping of each,
    /// `None` where it is unknown.
    pub(crate) const fn with_mapped(mut self, mapped: [Option<RawFileId>; 3]) -> Self {
        self.mapped = mapped;
        self
    }

    /// The lowest stream whose object is `id`.
    fn stream_of(&self, id: RawFileId) -> Option<RawFd> {
        self.objects
            .iter()
            .position(|stream| *stream == Some(id))
            .map(|stream| stream as RawFd)
    }

    /// The fixed inode of host object `id`, if it is one of the inherited
    /// stdio objects. Matches the full `(st_dev, st_ino)` pair: a file on
    /// another device with the same inode number (the third file of a fresh
    /// tmpfs, and /dev/null, are both inode 3) is a different object.
    pub(crate) fn inode_of_object(&self, id: RawFileId) -> Option<DetInode> {
        deterministic_stdio_inode(self.stream_of(id)?)
    }

    /// The fixed inode a descriptor derived from `stream` reports, having
    /// reached host object `reached` when that is known. That is the object's
    /// own fixed inode, which is the lower stream's for an object handed on
    /// two streams. A descriptor whose object is not recorded (hermit was
    /// started without that stream, or the identity could not be read)
    /// reports its stream's inode.
    pub(crate) fn inode_of_stream(&self, stream: RawFd, reached: Option<RawFileId>) -> DetInode {
        let recorded = usize::try_from(stream)
            .ok()
            .and_then(|index| self.objects.get(index).copied().flatten());
        let canonical = reached
            .and_then(|id| self.stream_of(id))
            .or_else(|| recorded.and_then(|id| self.stream_of(id)))
            .unwrap_or(stream);
        deterministic_stdio_inode(canonical).expect("a descriptor derives from stream 0, 1 or 2")
    }

    /// The fixed inode of the object a `/proc/<pid>/maps` header names, if it
    /// is an inherited stdio object.
    ///
    /// A header names the mapped inode by its superblock's device, which is
    /// not always the object's `st_dev`: btrfs reports one device there for
    /// every subvolume, and overlayfs maps the real layer file. So a header
    /// names a stdio object when it is the object's `(st_dev, st_ino)`, or the
    /// maps identity learnt for the object at startup by mapping it once (see
    /// `inherited_stdio_mapped_identity` in `tool_local`). Only when none was
    /// learnt (the object is not a regular file, could not be mapped there,
    /// or the backend runs inside the guest) does a header on another device
    /// name the object, with an equal inode number and a device that
    /// `paired(maps_device, stat_device)` says the pool has seen name one
    /// file.
    ///
    /// Residual (https://github.com/rrnewton/hermit/issues/3355): on btrfs a
    /// file in another subvolume of the same filesystem with the object's
    /// inode number has the object's maps identity, which the header cannot
    /// tell apart, and reports the fixed inode in maps.
    pub(crate) fn inode_of_mapping(
        &self,
        maps: RawFileId,
        paired: impl Fn(RawDevice, RawDevice) -> bool,
    ) -> Option<DetInode> {
        let named = self
            .objects
            .iter()
            .zip(self.mapped)
            .find_map(|(object, mapped)| {
                let object = (*object)?;
                let names = object == maps
                    || match mapped {
                        Some(mapped) => mapped == maps,
                        None => object.ino == maps.ino && paired(maps.dev, object.dev),
                    };
                names.then_some(object)
            })?;
        self.inode_of_object(named)
    }
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
    }

    /// The stream is decided by the resource, whatever the descriptor number:
    /// after `2>&1`, fd 2 and every duplicate of it carry stdout's resource.
    #[test]
    fn the_stream_is_decided_by_the_resource_not_the_descriptor_number() {
        for (stream, resource) in [
            ResourceID::Device(Device::ContainerStdin),
            ResourceID::Device(Device::ContainerStdout),
            ResourceID::Device(Device::ContainerStderr),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                inherited_stdio_stream(Some(resource)),
                Some(stream as RawFd)
            );
        }
        assert_eq!(inherited_stdio_stream(None), None);
        assert_eq!(
            inherited_stdio_stream(Some(ResourceID::FileContents(DetInode::mint(1001)))),
            None
        );
    }

    const DEV_NULL: RawFileId = RawFileId::new(7, 3);
    const TMPFS_THIRD: RawFileId = RawFileId::new(0x83, 3);
    const PIPE_THREE: RawFileId = RawFileId::new(0xe, 3);
    const PIPE: RawFileId = RawFileId::new(0xe, 3_907_000_111);
    const FILE: RawFileId = RawFileId::new(0x30, 7_917_682);

    #[test]
    fn an_object_matches_on_device_and_inode_not_the_inode_alone() {
        let stdio = InheritedStdio::new([Some(DEV_NULL), Some(PIPE), Some(FILE)]);
        assert_eq!(stdio.inode_of_object(DEV_NULL), Some(DetInode::mint(1000)));
        assert_eq!(stdio.inode_of_object(PIPE), Some(DetInode::mint(1001)));
        assert_eq!(stdio.inode_of_object(FILE), Some(DetInode::mint(1002)));
        // Inode 3 on a fresh tmpfs, or pipe inode 3, is not /dev/null.
        assert_eq!(stdio.inode_of_object(TMPFS_THIRD), None);
        assert_eq!(stdio.inode_of_object(PIPE_THREE), None);
        assert_eq!(
            stdio.inode_of_object(RawFileId::new(FILE.dev, PIPE.ino)),
            None
        );
    }

    /// One object handed on several streams has one identity: the lowest
    /// stream's inode, on every stream it was handed on.
    #[test]
    fn an_object_handed_on_several_streams_reports_the_lowest() {
        let terminal = RawFileId::new(0x19, 4);
        let tty = InheritedStdio::new([Some(terminal); 3]);
        for stream in 0..=2 {
            assert_eq!(
                tty.inode_of_stream(stream, Some(terminal)),
                DetInode::mint(1000)
            );
            assert_eq!(tty.inode_of_stream(stream, None), DetInode::mint(1000));
        }
        assert_eq!(tty.inode_of_object(terminal), Some(DetInode::mint(1000)));

        // `> F 2>&1`: stdout and stderr are F, stdin is /dev/null.
        let merged = InheritedStdio::new([Some(DEV_NULL), Some(FILE), Some(FILE)]);
        assert_eq!(
            merged.inode_of_stream(0, Some(DEV_NULL)),
            DetInode::mint(1000)
        );
        assert_eq!(merged.inode_of_stream(1, Some(FILE)), DetInode::mint(1001));
        assert_eq!(merged.inode_of_stream(2, Some(FILE)), DetInode::mint(1001));
        assert_eq!(merged.inode_of_stream(2, None), DetInode::mint(1001));
        assert_eq!(merged.inode_of_object(FILE), Some(DetInode::mint(1001)));
    }

    /// A stream hermit was started without, or whose identity could not be
    /// read, reports its own inode through a descriptor derived from it, and
    /// an object reached that was not recorded falls back to the stream's
    /// recorded object.
    #[test]
    fn an_unrecorded_stream_reports_its_own_inode() {
        let stdio = InheritedStdio::new([None, Some(PIPE), None]);
        assert_eq!(stdio.inode_of_stream(0, None), DetInode::mint(1000));
        assert_eq!(stdio.inode_of_stream(2, Some(FILE)), DetInode::mint(1002));
        assert_eq!(stdio.inode_of_stream(1, Some(FILE)), DetInode::mint(1001));
        assert_eq!(InheritedStdio::default().inode_of_object(PIPE), None);
    }

    /// A maps header names a stdio object only with its inode number AND its
    /// device, or a device the pool has seen paired with that device.
    #[test]
    fn a_mapping_matches_on_device_and_inode_or_a_paired_device() {
        let stdio = InheritedStdio::new([Some(DEV_NULL), Some(PIPE), Some(FILE)]);
        let never_paired = |_: RawDevice, _: RawDevice| false;
        assert_eq!(
            stdio.inode_of_mapping(FILE, never_paired),
            Some(DetInode::mint(1002))
        );
        assert_eq!(stdio.inode_of_mapping(TMPFS_THIRD, never_paired), None);

        // btrfs: the maps column names the file under another device.
        let maps_file = RawFileId::new(0x2f, FILE.ino);
        assert_eq!(stdio.inode_of_mapping(maps_file, never_paired), None);
        let btrfs = |maps: RawDevice, stat: RawDevice| (maps, stat) == (0x2f, 0x30);
        assert_eq!(
            stdio.inode_of_mapping(maps_file, btrfs),
            Some(DetInode::mint(1002))
        );
        // The pairing is of devices, not a licence to match any inode number.
        assert_eq!(
            stdio.inode_of_mapping(RawFileId::new(0x2f, DEV_NULL.ino), btrfs),
            None
        );
    }

    /// A maps identity learnt for an object at startup names it whatever the
    /// pool has paired, and replaces the pairing rule for that object.
    #[test]
    fn a_learnt_maps_identity_names_the_object_without_a_pairing() {
        // btrfs: `stat` names F under subvolume device 0x30, and its mapping
        // under the filesystem's device 0x2f. F is stdout and stderr.
        let maps_file = RawFileId::new(0x2f, FILE.ino);
        let stdio = InheritedStdio::new([Some(DEV_NULL), Some(FILE), Some(FILE)]).with_mapped([
            None,
            None,
            Some(maps_file),
        ]);
        let never_paired = |_: RawDevice, _: RawDevice| false;
        let always_paired = |_: RawDevice, _: RawDevice| true;
        // Learnt on stderr only, it still reports the lower stream's inode.
        assert_eq!(
            stdio.inode_of_mapping(maps_file, never_paired),
            Some(DetInode::mint(1001))
        );
        assert_eq!(
            stdio.inode_of_mapping(FILE, never_paired),
            Some(DetInode::mint(1001))
        );
        // Stdout has no learnt identity, so a paired device still names F
        // through it; with none paired, another device does not.
        let other_device = RawFileId::new(0x40, FILE.ino);
        assert_eq!(stdio.inode_of_mapping(other_device, never_paired), None);
        assert_eq!(
            stdio.inode_of_mapping(other_device, always_paired),
            Some(DetInode::mint(1001))
        );
        let learnt_everywhere = InheritedStdio::new([Some(DEV_NULL), Some(FILE), Some(FILE)])
            .with_mapped([None, Some(maps_file), Some(maps_file)]);
        assert_eq!(
            learnt_everywhere.inode_of_mapping(other_device, always_paired),
            None,
            "a learnt maps identity replaces the pairing rule"
        );
        // /dev/null has no learnt identity and keeps the pairing rule.
        assert_eq!(
            learnt_everywhere.inode_of_mapping(RawFileId::new(0x40, DEV_NULL.ino), always_paired),
            Some(DetInode::mint(1000))
        );
        assert_eq!(
            learnt_everywhere.inode_of_mapping(TMPFS_THIRD, never_paired),
            None
        );
    }
}
