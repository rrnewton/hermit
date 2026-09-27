# Maintained network providers

`package.rs` is the explicit offline build action. It requires the pinned
x86-64 Linux kernel/BTF contract, clang, llvm-objcopy, bpftool, libbpf development
files, and the cached rust-script dependencies. It does not load or attach BPF.
Cargo builds do not invoke these tools automatically.

For a build-tree installation, run the action with `--component unix-guard`
and `--output-dir <target>/debug/network-provider/unix-guard`; use `accepted`
for the accepted TCP provider. `--source-dir` selects this maintained source
directory. The CLI discovers only the package beside its actual executable or
`../lib/hermit/network-provider/unix-guard` in an installed prefix. The helper
executables and their dynamic-loader dependencies must be trusted immutable
installation inputs. Artifact hashes alone do not establish that trust.

Grouped accepted packages also contain the native PIE executable
`hermit-grouped-namespace-setup`, compiled from `grouped-namespace-setup.c` and
`grouped-namespace-policy.c` with their maintained policy header. Packaging
records all three source fingerprints and the exact `namespace_setup` filename
and `namespace_setup_sha256` in the manifest. Compilation uses the same bounded
offline compiler supervisor, strict warnings and a 1 MiB artifact limit; it
never executes the setup helper. The helper links libc, without libseccomp.
Grouped discovery refuses a missing, renamed, malformed or mismatched setup
member. Historical classic packages retain their original schema without this
member. Startup must retain the opened, revalidated setup file through launch;
the manifest hash does not authorize root execution or replace the trusted
immutable installation and fixed launcher policy.

The default Unix network policy currently needs a systemd host with BPF LSM
enabled, the exact qualified kernel ABI/BTF, and noninteractive authorization to
launch the fixed capability units and stop/query their exact names. The loader
runs as the invoking UID/GID with BPF, PERFMON, NET_ADMIN, SYS_RESOURCE and
SYS_PTRACE; it has neither DAC_OVERRIDE nor SYS_ADMIN. A separate bounded,
read-only helper has only SYS_ADMIN for original BPF-ID absence queries.

An administrator must provision a private bpffs directory owned by the invoking
UID, mode 0700. The default is `/sys/fs/bpf/hermit-<uid>`. The recovery directory
is `$XDG_STATE_HOME/hermit/network-guard` (or `$HOME/.local/state/hermit/network-guard`).
Deployments can instead pass the paired `--network-guard-bpffs DIRECTORY` and
`--network-guard-recovery DIRECTORY` options to `hermit run`. Both explicit
roots must already exist, be canonical directories, owned by the invoking UID,
and mode 0700; the first must be bpffs. Startup holds O_DIRECTORY/O_NOFOLLOW
references and transfers those references through the authenticated bootstrap.
Hermit never mounts bpffs, changes root ownership, or grants privilege because
an option contains a pathname. Recovery journals and bounded helper logs remain
in the recovery root. Each invocation writes its actual terminal receipt to
`<128-bit-run-id>.terminal.jsonl` there.

Missing/inaccessible roots, packages, privileges or incompatible kernels cause
a typed refusal before the guest starts. There is no live-network fallback.
These are material host requirements beyond ordinary unprivileged ptrace.
The helper monitors the actual held parent pidfd after a finite authenticated
bootstrap; channel aliases cannot extend that lifetime. The current CLI join is
qualified incrementally: non-ptrace enrollment and concurrent inherited Unix
readiness remain incomplete.
It refuses unsupported startup paths; a connect-denial result alone does not
establish full AF_UNIX isolation. A completed terminal receipt requires the
original 72 object IDs absent twice, actual helper/launcher/unit/cgroup drain,
and the original close-plus-one-second deadline. A successful guest also
requires its actual child wait and nonzero authenticated initial enrollment.
