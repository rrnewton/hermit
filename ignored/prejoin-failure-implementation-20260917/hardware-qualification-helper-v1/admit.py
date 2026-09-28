#!/usr/bin/python3
"""Require actual KVM admission inside the supervised test service."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess


def require(value, message):
    if not value:
        raise RuntimeError(message)


def digest(path):
    value = hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def write_new(path, value):
    with path.open('x') as output:
        json.dump(value, output, indent=2, allow_nan=False)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())


def check_binary(path, expected):
    path = Path(path)
    require(path.is_absolute() and not path.is_symlink(), 'invalid bound executable path')
    info = path.stat()
    require(stat.S_ISREG(info.st_mode) and os.access(path, os.X_OK), 'bound executable type or mode')
    require(digest(path) == expected, 'bound executable changed')
    return dict(path=str(path), sha256=expected, bytes=info.st_size, mode=info.st_mode & 0o7777)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--record', required=True)
    parser.add_argument('--executable', required=True)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--hermit-binary')
    parser.add_argument('--hermit-sha256')
    parser.add_argument('--guest-output')
    parser.add_argument('--target-root')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    require(args.arguments[:1] == ['--'], 'expected exact libtest argument separator')
    arguments = args.arguments[1:]
    require(len(arguments) == 5 and arguments[:4] ==
            ['--exact', '--nocapture', '--test-threads', '1'], 'changed libtest invocation')
    record = Path(args.record)
    require(record.is_absolute() and not record.exists() and not record.is_symlink(), 'retain admission record')
    binary = check_binary(args.executable, args.sha256)
    hermit = None
    if args.hermit_binary is not None:
        require(args.hermit_sha256 is not None, 'normal Hermit executable is unbound')
        hermit = check_binary(args.hermit_binary, args.hermit_sha256)
    else:
        require(args.hermit_sha256 is None, 'unexpected normal Hermit hash')
    if args.guest_output is not None:
        require(args.target_root is not None and hermit is not None, 'guest output lacks target or actual Hermit')
        target = Path(args.target_root)
        guest = Path(args.guest_output)
        require(target.is_absolute() and target.is_dir() and not target.is_symlink(), 'invalid owned target')
        require(target.stat().st_uid == os.getuid(), 'target has another owner')
        require(guest.is_absolute() and guest.is_relative_to(target), 'guest output escaped owned target')
        require(not guest.exists() and not guest.is_symlink(), 'retain prior compiled guest')
        for parent in guest.parents:
            if parent == target:
                break
            require(not parent.is_symlink(), 'guest output parent is a symlink')
    else:
        require(args.target_root is None, 'unexpected target root')
    device = os.open('/dev/kvm', os.O_RDWR | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        info = os.fstat(device)
        require(stat.S_ISCHR(info.st_mode), '/dev/kvm is not a character device')
        api = fcntl.ioctl(device, 0xAE00)
        require(api == 12, 'unsupported real KVM API version')
        with Path('/proc/self/cgroup').open() as source:
            cgroup = source.read(4097)
        require(len(cgroup) <= 4096, 'oversized service cgroup identity')
        require('safehermit-' in cgroup, 'test admission is outside the safehermit service')
        write_new(record, dict(device='/dev/kvm', device_inode=info.st_ino,
                  device_filesystem=info.st_dev, device_number=info.st_rdev,
                  api_version=api, supervisor_pid=os.getpid(), cgroup=cgroup,
                  test_binary=binary, hermit_binary=hermit,
                  argv=[args.executable, *arguments], guest_output=args.guest_output))
        # No new scope/session is created. Test, cc, timeout and actual Hermit
        # descendants inherit this observed safehermit service and its bounds.
        result = subprocess.run([args.executable, *arguments], stdin=subprocess.DEVNULL)
        current = os.stat('/dev/kvm', follow_symlinks=False)
        require((current.st_dev, current.st_ino, current.st_rdev) ==
                (info.st_dev, info.st_ino, info.st_rdev), 'KVM device changed during test')
        check_binary(args.executable, args.sha256)
        if hermit is not None:
            check_binary(args.hermit_binary, args.hermit_sha256)
        write_new(record.with_suffix('.exit.json'), dict(exit=result.returncode))
        raise SystemExit(result.returncode)
    finally:
        os.close(device)


if __name__ == '__main__':
    main()
