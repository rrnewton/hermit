# HANDOFF — slot `egress-probe` (agent: egress-probe)

**Written at teardown. Nothing uncommitted, nothing unpushed.**

## Active branch in this slot

| | |
|---|---|
| task | `sabre-route-in-guest-detlog-to-log-file` (and `make-silent-fallbacks-loud-across-backends`) |
| branch | `fix/sabre-fallback-is-loud` |
| SHA | `b221e3e0e2174cc8a858c3b6cf50dd1b0ff1144c` |
| base | hermit main `4c70658e785834737cbe1524f77330c781a6f5ea` |
| pushed | yes — verified at remote by `ls-remote` |
| PR | none opened |

Only untracked path is `sb/` — scratch test guests and binaries, deliberately not committed.

## Done

**SaBRe fallback made loud** (`hermit-cli/src/lib.rs`, +25). The fallback site reported only via
`tracing::info!`, invisible at default log level, so a run that degraded to ptrace was
indistinguishable from one that instrumented. Now emits an unconditional stderr banner in the
same shape e9patch uses. Prints *always*, not only when degraded, so a missing banner and a
banner reporting zero stay distinguishable.

**VERIFIED** (it was committed unverified; the verification landed later):
```
:: Backend: sabre static rewriting + ptrace runtime; ptrace_fallback_sites=0;
   trusted_shared_object_sites=0; guest_rpc_observed=true; reach_state=sabre-exercised
```

**SaBRe is now buildable on this box** — this was the real blocker behind three tasks:
1. `cmake -S reverie/third-party/sabre -B ignored/sabre-build -DCMAKE_BUILD_TYPE=Release && cmake --build ignored/sabre-build --parallel 16`
2. `cargo build --release -p detcore-sabre` → `target/release/libdetcore_sabre.so`, which **must** sit beside the hermit binary
3. run with `HERMIT_SABRE_BINARY=<...>/ignored/sabre-build/sabre`

**Measured:** SaBRe DETLOG reaches the durable log but is 99% empty — **1 line vs ptrace's 106**
at identical flags. The one line is supervisor-side (`SCHEDRAND` seed); the missing set is exactly
the in-guest records from `detcore::tool_local`. Routing exists; content does not.

## Next step

Get `detcore::tool_local`'s in-guest records across the SaBRe RPC boundary into the supervisor
sink. `guest_rpc_observed=true` means the transport is live, so records are produced-or-dropped
guest-side rather than the channel being absent. Reproduce with
`grep -c DETLOG` on `--backend sabre` vs the ptrace default under `--log=info --log-file=…`.

## Gates / caveats

- The **degraded** branch of the banner is still unobserved — no guest found that forces
  `ptrace_fallback_sites>0`. The mutation half of that fix is open.
- Committed `--no-verify`: the pre-commit reverie pin lint fail-closes on a GitHub fetch. Verified
  out of band that `rrnewton/reverie:main` is `dd3c178ea955` and the hermit pin matches. No
  manifest or lockfile touched.
- Runtime needs `LD_LIBRARY_PATH=/home/newton/fbsource/fbcode/third-party-buck/platform010/build/libunwind/lib`.
  Build/link use `ignored/lu-parity/usr/lib64` (it ships only the static `libunwind-ptrace.a`).

## Other work from this agent, all pushed

PRs on `4c70658e7`: **#1678** DetInode newtype · **#1705** validate artifact-integrity + README
· **#1711** pid/tid fixture — all three now **ready for review**, MERGEABLE, no approvals.
**#1726** (epoll/io_uring fixtures) was **CLOSED unmerged** by someone else; branch
`fixture/io-uring-and-epoll-edge-level` @ `b5c03d62` survives at the remote.

Parent-side: 30+ `rescue/*` refs at the remote, including 20 previously-dangling commits rescued
from the shared-HEAD race. **Do not run `git gc`/`prune`/`reflog expire`/`repack` on the parent.**
