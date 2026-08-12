# Preserved syscall probes

These five C sources were found as untracked work in `worktrees/egress-probe/hermit/sb/` on 2026-08-12. Their original task and intended product location were not recorded. This draft preserves the readable sources before any worktree reclaim; it does not claim that `sb/` is the correct final location or that these probes should land.

The six adjacent ELF files were deliberately not committed. Binaries are not permitted in this repository.

Measured with GCC 11.5.0:

| Source | Rebuild command | Original ELF SHA-256 | Result |
| --- | --- | --- | --- |
| `cred.c` | `cc cred.c -o cred` | `93e49870af84bdf1b69b2faa1bf7672f949fb3ced781cfab37faa8541833b34c` | byte-identical |
| `g.c` | `cc g.c -o g_dyn` | `14c5ec03a0dc8b1c27a5c1d26f6d76c1a3b1f034113dcef31779aaa3155ab1fb` | byte-identical |
| `g.c` | `cc -static g.c -o g_static` | `a477092a22297434701fe90188e244b29f9e675dfbd5b5e4e72a8452206c77df` | not byte-identical; same size and `main` disassembly, differing linked-library bytes/build ID |
| `inline.c` | `cc inline.c -o inline` | `fbffbafe705c3b12a5ceb158b06ee344f18c2f4651121bbbc6ede2789f41fba2` | byte-identical |
| `sysprobe.c` | `cc sysprobe.c -o sysprobe_dyn` | `14781071e8bf85c33595d247f248cd6dd8a28c78c4ed29acc89c15734275e20c` | byte-identical |
| `t.c` | `cc t.c -o t` | `d3247b20ddd2cbe20d099315bb147deae56f83d4fb175080732120b64db41066` | byte-identical |

The exact `g_static` binary remains only in the original slot pending an owner decision about external artifact retention. Nothing in this draft authorizes deletion or reclaim of that slot.
