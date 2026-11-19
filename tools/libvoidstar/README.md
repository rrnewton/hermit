# libvoidstar for Hermit

Programs written against the Antithesis SDKs take their randomness from one C
function, `fuzz_get_random()`, in a library named `libvoidstar.so` that the
SDKs load from `/usr/lib/libvoidstar.so`. `libvoidstar.c` here is Hermit's
guest-side version of that library.

Each `fuzz_get_random()` call is meant to be one `guest.random` choice point.
The call makes exactly one 8-byte `read` of the path `/dev/hermit/guest-random`
and returns the word. Hermit is to intercept that path and answer each read,
so that an outside search tool can pick the word and an ordinary run gets a
seeded, repeatable one. The path exists on no filesystem.

**Hermit does not intercept the path yet; that lands separately.** Until it
does, and always outside Hermit, the open fails and `fuzz_get_random()` writes
`hermit libvoidstar: /dev/hermit/guest-random: <reason>` to stderr and calls
`abort()`. It never falls back to `getrandom` or any other randomness.

## Build

```bash
cc -O2 -shared -fPIC -o libvoidstar.so libvoidstar.c
```

The file needs only libc: no `-pthread`, no other library.

## Delivery

Bind the built file read-only into the guest with Hermit's generic `--bind`
(see "File System, Environment, And Network" in `docs/USER_GUIDE.md`). The
SDKs need the fixed path `/usr/lib/libvoidstar.so`, and in host mode a bind
target must already exist, so run under `--image`, where the target is inside
the image's root filesystem:

```bash
hermit run --image=REF \
    --bind=/host/path/libvoidstar.so:/usr/lib/libvoidstar.so:ro -- PROGRAM
```

A harness of your own that loads the library by path can instead bind it
under the guest's `/tmp`, which host mode allows.

## Exports

Exactly the 14 symbols of the public Antithesis stub
([antithesishq/antithesis_stub](https://github.com/antithesishq/antithesis_stub),
`libvoidstar.c` at commit `00659dd02d9dfec3ade2f009239ec2addf344249`), with the
same signatures, and nothing else:

| Export | Behaviour here |
| --- | --- |
| `uint64_t fuzz_get_random(void)` | one 8-byte read of `/dev/hermit/guest-random`; aborts by name if the path cannot be opened or the read returns anything but 8 |
| `void fuzz_json_data(const char *, size_t)` | appends the record to `$ANTITHESIS_OUTPUT_DIR/sdk.jsonl` (below) |
| `void fuzz_json_data_unfilterable(const char *, size_t)` | the same |
| `void fuzz_flush(void)` | returns; it is not a receipt that any record reached a reader |
| `void __sanitizer_cov_trace_pc_guard_init(uint32_t *, uint32_t *)` | no-op; the guards stay zero |
| `void __sanitizer_cov_trace_pc_guard_internal(uint32_t *, uint64_t)` | no-op |
| `void __sanitizer_cov_trace_pc_guard(uint32_t *)` | no-op |
| `void __sanitizer_cov_pcs_init(const uintptr_t *, const uintptr_t *)` | no-op |
| `size_t init_coverage_module(size_t, const char *)` | returns `1 << 32` |
| `bool notify_coverage(size_t)` | returns `false` |
| `uint64_t notify_coverage_v2(size_t, uint64_t)` | returns `(0xFFFFF << 20) \| 0xFFFFF` |
| `const uint64_t *coverage_lease_generation_addr(void)` | the address of a constant 0, the same on every call |
| `uint64_t instrumentation_max_abi_version(void)` | returns 1 |
| `uint64_t instrumentation_request_abi_version(uint64_t)` | grants 1 for every request |

The coverage entry points record nothing; their return values are the stub's.

SDK records: when `ANTITHESIS_OUTPUT_DIR` is set and not empty, each record is
appended to `$ANTITHESIS_OUTPUT_DIR/sdk.jsonl` (created with mode 0644 if
absent) with its bytes unchanged, plus a newline if it does not end with one,
in a single `write(2)`. When the variable is unset or empty, records are
dropped and the process says so on stderr once. A failure to open or write the
file is reported on stderr once and never ends the process.

## ABI version 1

The stub reports version 3, the interface generation whose exports and return
values it implements. This library reports 1 because it does not implement the
coverage contract: its coverage entry points record nothing. What versions 1
and 2 promise to an instrumented binary is not documented by the stub, so
granting 1 to a binary that asks for more is unverified. The version rises
when coverage capture lands.

## Tests

`hermit-cli/tests/choice_driver.rs` compiles the library with `cc` and checks
it natively, without Hermit: the export list, the ABI answers, the coverage
values, the abort by name without the intercepted path, and the SDK record
file.
