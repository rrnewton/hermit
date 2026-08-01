// madvise(2) reclaim / lazy-free hint parity probe.
//
// This is the reclaim-hint sibling of the memory_advice row. memory_advice
// centers on MADV_DONTNEED restoration and MADV_WILLNEED (where KVM enforces a
// documented ENOSYS refusal for MADV_DONTNEED). This fixture instead exercises
// the newer reclaim / lazy-free advice values MADV_COLD (deactivate),
// MADV_PAGEOUT (reclaim), and MADV_FREE (lazy free) — which all three backends
// accept — plus madvise's faithful argument-validation error paths.
//
// The advice values are pure hints with no guest-observable data effect, so the
// contract asserts only the syscall return values and errno, never the content
// of an advised (reclaimed) region: reading a MADV_FREE'd page before rewriting
// it is genuinely undefined, so the fixture never does. The one content check
// reads the control half that received only MADV_COLD/MADV_PAGEOUT, whose
// contents the kernel preserves across reclaim.
//
// Two error paths are asserted because they are consistent across native and
// all three backends: a bogus advice value and a misaligned start address both
// yield EINVAL. The unmapped-range path is deliberately NOT asserted: native
// rejects it with EINVAL while the backends accept it, a backend-modeling
// divergence rather than a parity contract.
//
// All three backends and native pass all six checks (ok=6): madvise reclaim
// hints are faithfully forwarded, so this is a support contract, not a
// determinization override.

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef MADV_FREE
#define MADV_FREE 8
#endif
#ifndef MADV_COLD
#define MADV_COLD 20
#endif
#ifndef MADV_PAGEOUT
#define MADV_PAGEOUT 21
#endif

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    size_t len = (size_t)(4 * pg);
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) {
        printf("madvise_reclaim MMAP_FAIL\n");
        return 1;
    }
    memset(m, 'Z', len);

    int ok = 0;
    // (1) MADV_COLD deactivates the whole mapping without error.
    if (madvise(m, len, MADV_COLD) == 0) ok++;
    // (2) MADV_PAGEOUT reclaims the whole mapping without error.
    if (madvise(m, len, MADV_PAGEOUT) == 0) ok++;
    // (3) MADV_FREE lazily frees the second half without error.
    if (madvise(m + 2 * pg, (size_t)(2 * pg), MADV_FREE) == 0) ok++;
    // (4) Control half (only COLD/PAGEOUT) preserves its contents across
    //     reclaim; the MADV_FREE'd half is never read.
    if (m[0] == 'Z') ok++;
    // (5) A bogus advice value is rejected with EINVAL.
    if (madvise(m, (size_t)pg, 0x7abc) == -1 && errno == EINVAL) ok++;
    // (6) A misaligned start address is rejected with EINVAL.
    if (madvise(m + 1, (size_t)pg, MADV_COLD) == -1 && errno == EINVAL) ok++;

    munmap(m, len);
    printf("madvise_reclaim ok=%d\n", ok);
    return 0;
}
