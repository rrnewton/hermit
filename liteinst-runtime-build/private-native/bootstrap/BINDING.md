# Interpreter restoration record, HLBIND02

Little-endian, fixed 208-byte header followed by a bounded absolute target path
(1–4095 bytes and one NUL). Version1 records are not accepted. The record itself
must have all four immutable memfd seals; no pointers or synthesized guest stack.

Offsets0–151 retain the v1 layout: magic/version/size, four distinct descriptors
(runtime, original interpreter bytes, restoration O_PATH, record), current and
pre-clone namespaces, runtime inode, original inode/mode/size/timestamps and actual
cloned mount, alias attachment mount, and path length. Magic is HLBIND02, version2.

| Offset | Width | Meaning |
| --- | --- | --- |
| 152 | 8 | Actual runtime FD mount ID, including zero |
| 160 | 8 | Alias device `(major << 32) \| minor` |
| 168 | 8 | Alias inode, checked as a symlink |
| 176 | 4 | Fifth distinct FD: owned anonymous tmpfs source root |
| 180 | 4 | Reserved, must be zero |
| 184 | 8 | Source-root device |
| 192 | 8 | Source-root inode, checked as a directory |
| 200 | 8 | Actual source-root FD mount ID |

Source `runtime` stays linked in a detached tmpfs root, never a named guest
directory. The source root must be an O_PATH tmpfs capability; its exact identity,
symlink inode and `/proc/self/fd/<runtime>` contents are checked. The alias mount
and resolved runtime identities are independent, never interchangeable.

The real same-runtime mapping proof precedes restoration. Then verify namespace,
all capabilities, source and top alias; detach with UMOUNT_NOFOLLOW; verify original
path/permissions; unlink source and verify absence; close the source-root,
restoration and record FDs. Only then can restoration_complete permit private CRT.
Runtime/original-image FDs remain owned by the existing startup consumer.
No source unlink occurs after failed restoration. The host owner retains all
resources through reaping, retrying checked restoration on failed startup; an
unconfirmed reap retains them rather than running cleanup. Terminal namespace/
anonymous-mount disposal needs no guest-visible pathname removal.

Production consumers and tests are under this private-native component; historical
external bootstrap copies are not interchangeable build or test inputs.
