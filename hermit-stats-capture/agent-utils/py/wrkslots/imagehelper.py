"""Privileged operations on slot images, run through ``sudo -n`` (internal).

``python -I imagehelper.py OPERATION ARGS...`` as root, invoked by
:mod:`wrkslots.slotimage` for the kernel backend. Paths come from the
invoking user, so nothing is trusted by name:

* the invoking user is SUDO_UID, as recorded by sudo;
* every path is opened once with O_NOFOLLOW and checked through its
  descriptor (type, owner, and for a mount point: an empty, unmounted
  directory owned by the user);
* the operation then acts on the descriptor (``/proc/self/fd/N``), never on
  the path again, so a path swapped after the check cannot redirect it;
* operations on an existing mount require a loop mount whose backing file
  the user owns, at the root of its file system.

Operations: ``mount IMAGE MOUNT-POINT``, ``umount MOUNT-POINT``,
``rmdir-lost-found MOUNT-POINT``, ``trim MOUNT-POINT``, ``grow MOUNT-POINT``.
"""

from __future__ import annotations

import ctypes
import errno
import fcntl
import os
import stat
import struct
import subprocess
import sys
from collections.abc import Sequence

_LOOP_SET_FD = 0x4C00
_LOOP_CLR_FD = 0x4C01
_LOOP_SET_STATUS64 = 0x4C04
_LOOP_SET_CAPACITY = 0x4C07
_LOOP_CONFIGURE = 0x4C0A
_LOOP_CTL_GET_FREE = 0x4C82
_LO_FLAGS_AUTOCLEAR = 4
_FITRIM = 0xC0185879
_LOOP_INFO64 = "=QQQQQIIII64s64s32sQQ"
_O_PATH = 0o10000000
_MS_NOSUID = 2
_MS_NODEV = 4
_MS_NOATIME = 1024
_EXT4_ROOT_INODE = 2
_LOOP_MAJOR = 7


class HelperError(RuntimeError):
    """A privileged image operation was refused or failed."""


def _libc() -> ctypes.CDLL:
    return ctypes.CDLL(None, use_errno=True)


def _check(result: int, what: str) -> None:
    if result != 0:
        error = ctypes.get_errno()
        raise HelperError(f"{what}: {os.strerror(error)}")


def _invoking_uid() -> int:
    if os.geteuid() != 0:
        raise HelperError("the image helper must run as root (through sudo -n)")
    try:
        uid = int(os.environ["SUDO_UID"])
    except (KeyError, ValueError) as exc:
        raise HelperError("the image helper must run through sudo (SUDO_UID is missing)") from exc
    if uid == 0:
        raise HelperError("refusing to manage slot images for root")
    return uid


def _open_mount_point(path: str, uid: int) -> int:
    """Pin a directory that is about to become a mount point: real, empty, unmounted, the user's."""

    descriptor = os.open(path, _O_PATH | os.O_NOFOLLOW | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        info = os.fstat(descriptor)
        parent = os.stat("..", dir_fd=descriptor, follow_symlinks=False)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != uid:
            raise HelperError(f"mount point is not a directory owned by the invoking user: {path}")
        if info.st_dev != parent.st_dev or (info.st_dev, info.st_ino) == (parent.st_dev, parent.st_ino):
            raise HelperError(f"mount point already has a file system mounted on it: {path}")
        if os.listdir(f"/proc/self/fd/{descriptor}"):
            raise HelperError(f"mount point is not empty: {path}")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _loop_backing(device: int) -> str:
    base = f"/sys/dev/block/{os.major(device)}:{os.minor(device)}"
    try:
        with open(f"{base}/loop/backing_file", encoding="utf-8") as handle:
            return handle.read().strip()
    except OSError as exc:
        raise HelperError("not a loop-device mount") from exc


def _loop_name(device: int) -> str:
    base = f"/sys/dev/block/{os.major(device)}:{os.minor(device)}/uevent"
    with open(base, encoding="utf-8") as handle:
        for line in handle:
            if line.startswith("DEVNAME="):
                return "/dev/" + line.split("=", 1)[1].strip()
    raise HelperError("cannot name the loop device")


def _open_owned_mount(path: str, uid: int, flags: int = _O_PATH) -> int:
    """Pin the root of a mounted slot image the invoking user owns."""

    descriptor = os.open(path, flags | os.O_NOFOLLOW | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        info = os.fstat(descriptor)
        if os.major(info.st_dev) != _LOOP_MAJOR or info.st_ino != _EXT4_ROOT_INODE:
            raise HelperError(f"{path} is not the root of a loop-mounted slot image")
        backing = _loop_backing(info.st_dev)
        owner = os.stat(backing).st_uid
        if owner != uid:
            raise HelperError(f"the image mounted at {path} is not owned by the invoking user")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _configure_loop(image: int) -> tuple[int, str]:
    """Attach an open image file to a free loop device with autoclear; return (fd, name)."""

    control = os.open("/dev/loop-control", os.O_RDWR | os.O_CLOEXEC)
    try:
        for _attempt in range(8):
            number = fcntl.ioctl(control, _LOOP_CTL_GET_FREE)
            name = f"/dev/loop{number}"
            device = os.open(name, os.O_RDWR | os.O_CLOEXEC)
            info = struct.pack(_LOOP_INFO64, 0, 0, 0, 0, 0, 0, 0, 0, _LO_FLAGS_AUTOCLEAR, b"", b"", b"", 0, 0)
            try:
                config = struct.pack("=II", image, 0) + info + bytes(64)
                fcntl.ioctl(device, _LOOP_CONFIGURE, config)
                return device, name
            except OSError as exc:
                if exc.errno == errno.EBUSY:
                    os.close(device)
                    continue
                if exc.errno not in (errno.EINVAL, errno.ENOTTY):
                    os.close(device)
                    raise
            try:
                fcntl.ioctl(device, _LOOP_SET_FD, image)
            except OSError as exc:
                os.close(device)
                if exc.errno == errno.EBUSY:
                    continue
                raise
            try:
                fcntl.ioctl(device, _LOOP_SET_STATUS64, info)
            except OSError:
                fcntl.ioctl(device, _LOOP_CLR_FD)
                os.close(device)
                raise
            return device, name
    finally:
        os.close(control)
    raise HelperError("no free loop device")


def mount(image_path: str, mount_point: str) -> None:
    """Attach a user-owned image to a loop device and mount it on a pinned, empty directory."""
    uid = _invoking_uid()
    image = os.open(image_path, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        info = os.fstat(image)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != uid or info.st_nlink != 1:
            raise HelperError(f"image is not a singly linked regular file owned by the invoking user: {image_path}")
        target = _open_mount_point(mount_point, uid)
        try:
            device, name = _configure_loop(image)
            try:
                libc = _libc()
                libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
                result = libc.mount(
                    name.encode(),
                    f"/proc/self/fd/{target}".encode(),
                    b"ext4",
                    _MS_NOSUID | _MS_NODEV | _MS_NOATIME,
                    b"discard",
                )
                if result != 0:
                    error = ctypes.get_errno()
                    fcntl.ioctl(device, _LOOP_CLR_FD)
                    raise HelperError(f"mount {image_path} on {mount_point}: {os.strerror(error)}")
            finally:
                os.close(device)
        finally:
            os.close(target)
    finally:
        os.close(image)


def umount(mount_point: str) -> None:
    """Unmount a user-owned loop-mounted slot image."""
    uid = _invoking_uid()
    descriptor = _open_owned_mount(mount_point, uid)
    identity = os.fstat(descriptor)
    os.close(descriptor)
    # An open descriptor would keep the mount busy. Re-check the same mount by
    # path without following a symbolic link, then detach exactly that path.
    again = os.lstat(mount_point)
    if (again.st_dev, again.st_ino) != (identity.st_dev, identity.st_ino):
        raise HelperError(f"{mount_point} changed while it was checked")
    _check(_libc().umount2(mount_point.encode(), 8), f"umount {mount_point}")  # UMOUNT_NOFOLLOW


def rmdir_lost_found(mount_point: str) -> None:
    """Remove the root-owned lost+found directory mkfs leaves at a slot image's root."""
    uid = _invoking_uid()
    descriptor = _open_owned_mount(mount_point, uid)
    try:
        os.rmdir("lost+found", dir_fd=descriptor)
    except FileNotFoundError:
        pass
    finally:
        os.close(descriptor)


def trim(mount_point: str) -> None:
    """Return blocks freed inside a mounted slot image to the host (FITRIM)."""
    uid = _invoking_uid()
    descriptor = _open_owned_mount(mount_point, uid, os.O_RDONLY)
    try:
        fcntl.ioctl(descriptor, _FITRIM, struct.pack("=QQQ", 0, 0xFFFFFFFFFFFFFFFF, 0))
    finally:
        os.close(descriptor)


def grow(mount_point: str) -> None:
    """Pick up a larger image file on its loop device and grow the file system online."""
    uid = _invoking_uid()
    descriptor = _open_owned_mount(mount_point, uid, os.O_RDONLY)
    try:
        name = _loop_name(os.fstat(descriptor).st_dev)
        device = os.open(name, os.O_RDONLY | os.O_CLOEXEC)
        try:
            fcntl.ioctl(device, _LOOP_SET_CAPACITY)
        finally:
            os.close(device)
        result = subprocess.run([_system_tool("resize2fs"), name], capture_output=True, text=True, check=False, timeout=600)
        if result.returncode != 0:
            raise HelperError(f"resize2fs {name}: {(result.stderr or result.stdout).strip()}")
    finally:
        os.close(descriptor)


def _system_tool(name: str) -> str:
    """A root-owned system executable by absolute path: never resolved on the caller's PATH."""

    for directory in ("/usr/sbin", "/sbin", "/usr/bin", "/bin"):
        candidate = os.path.join(directory, name)
        try:
            info = os.stat(candidate)
        except OSError:
            continue
        if info.st_uid == 0 and not info.st_mode & 0o022 and os.access(candidate, os.X_OK):
            return candidate
    raise HelperError(f"{name} was not found as a root-owned executable in the system directories")


_OPERATIONS = {
    "mount": (mount, 2),
    "umount": (umount, 1),
    "rmdir-lost-found": (rmdir_lost_found, 1),
    "trim": (trim, 1),
    "grow": (grow, 1),
}


def _main(argv: Sequence[str] | None = None) -> int:
    """Run one operation named on the command line; return the exit status."""
    values = list(sys.argv[1:] if argv is None else argv)
    if not values or values[0] not in _OPERATIONS or len(values) - 1 != _OPERATIONS[values[0]][1]:
        print("usage: imagehelper.py mount IMAGE MOUNT-POINT | umount|rmdir-lost-found|trim|grow MOUNT-POINT", file=sys.stderr)
        return 2
    operation, _count = _OPERATIONS[values[0]]
    try:
        operation(*values[1:])  # type: ignore[operator]
    except (HelperError, OSError) as exc:
        print(f"wrkslots image helper: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
