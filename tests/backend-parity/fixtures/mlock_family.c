#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

/*
 * Memory-residency locking round-trip via mlock(2), munlock(2), and mlock2(2).
 * This is a distinct memory family from the mapping-layout rows (which compare
 * address sequences) and the memory-advice row (which drives madvise hints): it
 * pins a private anonymous mapping into RAM and releases it again.
 *
 * The contract locks and unlocks a fixed two-page anonymous region three ways
 * and asserts only the return values, never an address or a residency count, so
 * the golden stdout is portable:
 *   1. mlock the whole region succeeds.
 *   2. munlock releases it.
 *   3. mlock2 with no flags (whole-region prefault) succeeds.
 *   4. munlock releases it.
 *   5. mlock2 with MLOCK_ONFAULT (lock-on-fault) succeeds.
 *   6. munlock releases it.
 *
 * Locking a small self-owned mapping needs no privilege under the usual
 * RLIMIT_MEMLOCK, and locking has no guest-visible data side effect, so the
 * result depends on no host state. mlockall/munlockall are deliberately omitted
 * because locking every current mapping perturbs a backend's own runtime pages.
 */
int main(void) {
    int ok = 0;
    long pg = sysconf(_SC_PAGESIZE);
    size_t sz = (size_t)pg * 2;
    void *p = mmap(NULL, sz, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("mlock ok=0\n");
        return 0;
    }

    if (mlock(p, sz) == 0) {
        ok++;
    }
    if (munlock(p, sz) == 0) {
        ok++;
    }
    if (mlock2(p, sz, 0) == 0) {
        ok++;
    }
    if (munlock(p, sz) == 0) {
        ok++;
    }
    if (mlock2(p, sz, MLOCK_ONFAULT) == 0) {
        ok++;
    }
    if (munlock(p, sz) == 0) {
        ok++;
    }

    munmap(p, sz);
    printf("mlock ok=%d\n", ok);
    return 0;
}
