#!/usr/bin/env python3
"""Build the QEMU demo initramfs with stand-in tools and check what it stores.

demos/lib/qemu-assets.sh builds the BusyBox initramfs that demos 5, 6, and 7
boot. These tests run the real script with stand-ins for QEMU, qemu-img,
file(1), the kernel image, and BusyBox, so they need no QEMU, no BusyBox, and
no network. They do run the host's cpio and gzip, which the script requires.
"""

import gzip
import hashlib
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DEMO_DIR = Path(__file__).resolve().parent.parent
QEMU_ASSETS_SCRIPT = DEMO_DIR / "lib" / "qemu-assets.sh"

# The stand-in BusyBox answers only the question qemu-assets.sh asks it. The
# applet usr/local/bin/extra makes the script create a directory of its own.
FAKE_BUSYBOX = """#!/bin/sh
if [ "$1" = --list-full ]; then
  printf '%s\\n' bin/sh sbin/poweroff usr/bin/env usr/local/bin/extra
  exit 0
fi
exit 1
"""

# The script refuses a BusyBox that file(1) does not call statically linked.
FAKE_FILE = """#!/bin/sh
printf '%s: ELF 64-bit LSB executable, x86-64, statically linked\\n' "$1"
"""

DIRECTORY_MODE = stat.S_IFDIR | 0o755
EXECUTABLE_MODE = stat.S_IFREG | 0o755
DATA_FILE_MODE = stat.S_IFREG | 0o644
SYMLINK_MODE = stat.S_IFLNK | 0o777


def _write_executable(path: Path, text: str, mode: int = 0o755) -> None:
    path.write_text(text)
    path.chmod(mode)


def parse_newc(archive: bytes) -> dict:
    """Map each entry name of a newc cpio archive to its stored mode."""
    modes = {}
    offset = 0
    while True:
        header = archive[offset : offset + 110]
        if header[:6] != b"070701":
            raise ValueError("no newc header at offset {}".format(offset))
        fields = [int(header[6 + 8 * i : 14 + 8 * i], 16) for i in range(13)]
        mode, file_size, name_size = fields[1], fields[6], fields[11]
        name_start = offset + 110
        name = archive[name_start : name_start + name_size - 1].decode()
        offset = (name_start + name_size + 3) & ~3
        if name == "TRAILER!!!":
            return modes
        if name.startswith("./"):
            name = name[2:]
        modes[name] = mode
        offset = (offset + file_size + 3) & ~3


class InitramfsBuildTest(unittest.TestCase):
    def _build(self, umask: int, busybox_mode: int = 0o755) -> bytes:
        """Run qemu-assets.sh under `umask` and return the initramfs bytes."""
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        scratch = Path(holder.name)
        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        _write_executable(fake_bin / "file", FAKE_FILE)
        _write_executable(fake_bin / "qemu-img", "#!/bin/sh\nexit 0\n")
        _write_executable(fake_bin / "qemu-system-x86_64", "#!/bin/sh\nexit 0\n")
        busybox = scratch / "busybox"
        _write_executable(busybox, FAKE_BUSYBOX, busybox_mode)
        kernel = scratch / "bzImage"
        kernel.write_bytes(b"stand-in kernel image\n")
        assets = scratch / "assets"
        repo = scratch / "repo"
        repo.mkdir()

        env = dict(os.environ)
        env.update(
            PATH="{}:{}".format(fake_bin, env.get("PATH", "/usr/bin:/bin")),
            QEMU_ASSETS=str(assets),
            HERMIT_REPO=str(repo),
            BUSYBOX=str(busybox),
            KERNEL_IMAGE=str(kernel),
            QEMU_KERNEL_SHA256=hashlib.sha256(kernel.read_bytes()).hexdigest(),
            QEMU_BIN=str(fake_bin / "qemu-system-x86_64"),
            QEMU_DEMO_PYTHON=sys.executable,
        )
        result = subprocess.run(
            ["bash", str(QEMU_ASSETS_SCRIPT)],
            env=env,
            cwd=scratch,
            umask=umask,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(
            0,
            result.returncode,
            "qemu-assets.sh failed under umask {:03o}:\n{}{}".format(
                umask, result.stdout, result.stderr
            ),
        )
        return gzip.decompress((assets / "initramfs.cpio.gz").read_bytes())

    def _assert_fixed_modes(self, modes: dict) -> None:
        for name in (
            ".",
            "bin",
            "dev",
            "etc",
            "proc",
            "sbin",
            "sys",
            "tmp",
            "usr",
            "usr/bin",
            "usr/local",
            "usr/local/bin",
            "usr/sbin",
        ):
            self.assertEqual(
                oct(DIRECTORY_MODE), oct(modes[name]), "directory /" + name
            )
        for name in ("bin/busybox", "init"):
            self.assertEqual(oct(EXECUTABLE_MODE), oct(modes[name]), "/" + name)
        for name in ("etc/group", "etc/passwd"):
            self.assertEqual(oct(DATA_FILE_MODE), oct(modes[name]), "/" + name)
        for name in ("bin/sh", "sbin/poweroff", "usr/bin/env", "usr/local/bin/extra"):
            self.assertEqual(oct(SYMLINK_MODE), oct(modes[name]), "/" + name)

    def test_a_restrictive_umask_does_not_change_the_stored_modes(self):
        self._assert_fixed_modes(parse_newc(self._build(umask=0o077)))

    def test_the_archive_is_byte_identical_under_umask_022_and_077(self):
        # Both builds run in this test's temporary directories, so they are on
        # the same filesystem: cpio also stores each directory's link count,
        # which depends on the filesystem.
        usual = self._build(umask=0o022)
        restrictive = self._build(umask=0o077)
        self.assertEqual(parse_newc(usual), parse_newc(restrictive))
        self.assertEqual(
            hashlib.sha256(usual).hexdigest(), hashlib.sha256(restrictive).hexdigest()
        )

    def test_the_installed_busybox_mode_does_not_reach_the_archive(self):
        # A BusyBox from a read-only store, such as Nix's, is installed 0555.
        modes = parse_newc(self._build(umask=0o022, busybox_mode=0o555))
        self._assert_fixed_modes(modes)


if __name__ == "__main__":
    unittest.main()
