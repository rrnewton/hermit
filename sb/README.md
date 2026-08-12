# Preserved egress probe sources

These five C files were recovered from the uncommitted `sb/` directory in the
legacy `egress-probe` worktree on `devbig014`. This draft preserves evidence; it
does not claim that the probes are production tests or validated Hermit inputs.

Source SHA-256 values:

- `cred.c`: `890351ec2b221c2f88d44d98ff857632e4cf0888144a86200379d07e93ba8b8d`
- `g.c`: `0650513dfa2b314cd0f5cc808877a4768d2652aab6735315682093aeb33f2999`
- `inline.c`: `caa3a90ef43ab0d44e8f625275e9f8072e37870dff454da5a98f92f576a9385d`
- `sysprobe.c`: `0650513dfa2b314cd0f5cc808877a4768d2652aab6735315682093aeb33f2999`
- `t.c`: `c6ddabdcdb1ef105060b4c902ffd015da67b22f09e3e72234ee3df65dd0f013a`

The observed compiler was `cc (GCC) 11.5.0 20240719 (Red Hat 11.5.0-15)`.
These commands reproduced five adjacent ELF outputs byte-for-byte:

```text
cc cred.c -o cred
cc g.c -o g_dyn
cc inline.c -o inline
cc sysprobe.c -o sysprobe_dyn
cc t.c -o t
```

The original output hashes were:

- `cred`: `93e49870af84bdf1b69b2faa1bf7672f949fb3ced781cfab37faa8541833b34c`
- `g_dyn`: `14c5ec03a0dc8b1c27a5c1d26f6d76c1a3b1f034113dcef31779aaa3155ab1fb`
- `inline`: `fbffbafe705c3b12a5ceb158b06ee344f18c2f4651121bbbc6ede2789f41fba2`
- `sysprobe_dyn`: `14781071e8bf85c33595d247f248cd6dd8a28c78c4ed29acc89c15734275e20c`
- `t`: `d3247b20ddd2cbe20d099315bb147deae56f83d4fb175080732120b64db41066`

The sixth output, `g_static`, had SHA-256
`a477092a22297434701fe90188e244b29f9e675dfbd5b5e4e72a8452206c77df`.
`cc -static g.c -o g_static` produced a different binary despite the same size
and identical `main` disassembly; optimization variants also differed. No ELF
is committed here because repository policy forbids generated binaries.
