Follow-up to https://github.com/rrnewton/hermit/pull/4709 under the coordinator waiver recorded 2026-11-05 3:19 AM ET. At the waived head 9a596880d28d5bea14d735de170eefc34786ed13, CPU-profile Auto does not yet resolve the initial image and every script/ELF interpreter wholly inside the launch's completed mount/root/cwd view at every launch boundary.

Measured witnesses (all false named refusals, exit 122, on the measured path; explicit x86-64-v2 ran):
- a symlink alias into a file-bind target;
- a relative script interpreter (#!./interp) under --workdir;
- an unmarked source bound over a v4-declaring host target at the public library admission boundary.

Qualified behaviour: fail-closed under the ptrace backend by Detcore's per-exec ISA check. Not a universal cross-backend guarantee: isa_refusal is skipped for declared launchers / no CPUID interception (detcore/src/syscalls/namespace.rs:1746-1748), public KVM ignores Command mounts/chroot (hermit-cli/src/lib.rs:3131-3134), and later KVM exec checking is disclosed absent (detcore/src/syscalls/namespace.rs:1731-1738). No silent under-selection witness has been established.

Fix route: land Reverie Command mount/chroot (and completed cwd/root) accessors, bump the Hermit Reverie pin, resolve the initial image and every script/ELF interpreter through the completed launch view at record/run and public library boundaries, and add revcodex's public file-bind regression plus red-first tests for the alias and relative-interpreter witnesses. Explicit named profiles must keep working unchanged.
