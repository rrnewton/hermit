# gVisor raw-getpid comparison handoff

No product source was changed. This clean Hermit checkout is at
`9c80e2b29d8de2864fc0c17eb8da34a8c96a9650`; the release binary built from it
has SHA-256
`f19ca28fdae63cbc3cab4687d7d149439d6371babe30000f032236b9d3e7ba2d`.

## Retained timing evidence

The accepted same-host timing comparison is outside this slot at
`/home/newton/work/dev-hermit/ignored/validate/evidence/benchmark-kvm-vs-gvisor-syscall-20260905/run-current2-8c8c0a57/`.
It ran 15 fresh processes per arm, with two 10,000-call warmups and a bracketed
100,000-call raw `SYS_getpid` measurement per process, on reserved CPU 3 with
zero cgroup throttling. Median wall/CPU microseconds per call were:

- native: 0.076349 / 0.076015
- Hermit ptrace: 29.466249 / 29.407145
- Hermit KVM: 3.681154 / 3.673170
- gVisor systrap: 5.535034 / 5.657540
- gVisor KVM: 0.790930 / 0.783945

Hermit KVM was 4.654206x gVisor KVM in wall time and 4.685495x in CPU time.
This is timing evidence only, not a profile. The accepted evidence hashes are:

- `README.md`: `10febdd899c9ad695d7a929481877584555e586ee63103b5a65c59695b9b9abb`
- `metadata.json`: `80a612ede92b2033b64bbbe4c3afea69d61adc4f6ca47bf263b76944d8ee96eb`
- `raw.csv`: `f7b3f2a01d78e139066af2748b7e206dc5ec2e3915dd31dd72ae38dcf3f5d62e`
- `pairs.csv`: `7075345cf470c4a1f40f76f6de633f8cc95cb2fac775fe4eb299e3225fffcaf3`
- admission record: `3231afe3d7233f4edb9eb486a759fbf52a2c91e366dd4802d32f7e0c5be14b7d`
- fixture source: `c83ae1cfcaa264809fac5e66c4d7e037b3047c0ab372142fea3c3aa53be5a77f`
- fixture binary: `634c70472773839cd5c4f23e1765a519664a8381baa67b8c7ad68e3cbbe56eae`
- runsc release 20260727.0: `6ec46808a22c94b7ea68dd9521e831b44c69e0d3267a2cc862f9a6ae290cee91`

## Profiling evidence and exclusions

The durable profiling write-up is
`/home/newton/work/dev-hermit/ignored/validate/evidence/kvm-gvisor-getpid-profile-20260906/README.md`,
SHA-256 `eb8387458e5c66b682ba9fbd5aabce2f254d688d3f1e760f7fdb45fee53a4865`.
Its final harness SHA-256 is
`1142a49303417c59b3232b80b712d63c414ff812a6a637fdd8752e224d87da82`.

The second admitted attempt completed one valid Hermit-KVM-only 500,000-call
profile, then failed while finalizing the gVisor KVM profile. The gVisor data is
excluded because `perf report` reports a zero data-size field and fails to
process samples. Ptrace and gVisor systrap did not run. Admission record:
`benchmark-kvm-syscall-20260906T063843Z-e586be.json`, SHA-256
`8326072be087c6ea19b5c1a35e775f12fb5cb5f7d82aa3a6ae519d2b76ba1a70`.

The third and final admitted attempt is wholly excluded. Perf's fd-control
channel returned five bytes (`ack`, newline, NUL), while the harness required
exactly `ack`; it stopped after KVM preflight and two 10,000-call warmups and
before the 500,000-call request. No complete arm ran. Admission record:
`benchmark-kvm-syscall-20260906T064312Z-c98166.json`, SHA-256
`945cbba1a9b232c346d36773a6790194a28fe8b884593bbde0e3b50fa03bb0c5`.

The first admission launched no backend because the original output guard
rejected the admission launcher's own `admission.log`. Its record is
`benchmark-kvm-syscall-20260906T063719Z-c2875a.json`, SHA-256
`95f3373e2942b5e6c6e4e2c91515fc27e0151ec0f6c37df9edc82965774ae63f`.

## Valid Hermit-only perf artifact

The usable profile is
`/home/newton/work/dev-hermit/ignored/validate/evidence/kvm-gvisor-getpid-profile-20260906/run-9c80e2b2-r2/profile-kvm.data`,
SHA-256 `cd8d1ead9cebe0dc6e3d014935e1e6937b0a91ab07c0f0c852c8e926f1dc3641`.
It contains 1,822 `cpu-clock` samples, zero lost samples, and an approximately
1.826-second sample span. Its flat and inclusive reports have SHA-256
`eadace5f5849ffde68610f550bbe6074f60e1aac0843ed0ffe23442a6bf10c9d`
and `e79990da1d3cb04d0c81847413169458d03c04024bd6b98bfd9110c9ffe9d065`.
The largest inclusive paths are the host syscall boundary (29.64%), `ioctl`
(28.87%), `VcpuFd::run` (25.25%), FPU-state switching (11.58%),
`KvmBackend::run_static_elf_process_with_tool` (7.46%),
`FileTableState::install` (7.41%), and `VcpuFd::get_regs` (4.77%).

This artifact may be reported as a Hermit KVM profile only. It does not support
a Hermit-versus-gVisor profiling comparison.
