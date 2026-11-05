# tick-hub adversarial review — 2026-08-05

An agent that did not author the initial port reviewed both implementations,
their public docs, and their independently built packages.

This review record entered the repository with implementation commit
`5ef91c55036227b5dc2997ef069784f337b4cc5e`.

## Findings resolved

1. Malformed or reserved YAML state could leak a parser traceback. Both paths
   now report the documented controlled state error.
2. A timed-out shell gate could leave background descendants alive. The command
   now runs in its own process group, the group is killed on timeout, both pipes
   are drained, and invalid output bytes are decoded consistently.
3. Fired-state persistence accepted invalid keys and bool, negative, or
   out-of-range epochs, and temporary files could survive a failed rename.
   Both implementations validate the same signed-64-bit domain and clean failed
   atomic writes.
4. Directly constructed invalid state could serialize into a document the tool
   could not reload. Serializers now validate and canonicalize their models.
5. Documentation promised interpolation from static and captured fields, while
   both engines used captured values only. Both now implement the documented
   merge and exact field ordering.
6. Public module and package documentation contained future-port or
   sibling-language wording. Installed documentation now describes only the
   package the reader installed.

## Evidence

- Focused Python suite and dependency-free smoke tests: 100 passed.
- Native suite: 44 passed; formatting, Clippy with warnings denied, and rustdoc
  with warnings denied were clean.
- Differential: 1,040/1,040 checks with 500 randomized fixtures and seed
  `20260805`.
- The wheel was built and installed without dependencies outside the source
  tree; its command and public API worked from an unrelated directory.
- The registry package contained 19 files and passed Cargo's normalized-package
  compile verification.

Reproduce the extended differential with:

```sh
python3 cross/differential.py --tool tick-hub --random 500 --seed 20260805
```
