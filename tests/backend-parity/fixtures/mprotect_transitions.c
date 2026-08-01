#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/*
 * Memory-protection transitions via mprotect(2) on a private anonymous mapping.
 * This is distinct from the executable-memory row (which drives the W^X
 * writable->executable transition and calls into the mapping): here the mapping
 * is never made executable, and the contract cycles read/write protections and
 * PROT_NONE, touching memory only where the current protection permits it, so no
 * fault is ever raised.
 *
 * It asserts only observable read/write outcomes, never an address, so the
 * golden stdout is portable:
 *   1. a fresh RW mapping is writable ('A' stored and read back).
 *   2. mprotect to PROT_READ succeeds.
 *   3. the page is still readable while read-only.
 *   4. mprotect back to PROT_READ|PROT_WRITE succeeds.
 *   5. the page is writable again ('B' stored and read back).
 *   6. mprotect to PROT_NONE succeeds.
 *   7. mprotect back to PROT_READ|PROT_WRITE restores access.
 */
int main(void) {
    int ok = 0;
    long pg = sysconf(_SC_PAGESIZE);
    size_t sz = (size_t)pg;
    char *p = mmap(NULL, sz, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("mprot ok=0\n");
        return 0;
    }

    memset(p, 'A', sz);
    if (p[0] == 'A') {
        ok++;
    }
    if (mprotect(p, sz, PROT_READ) == 0) {
        ok++;
    }
    if (p[0] == 'A') {
        ok++;
    }
    if (mprotect(p, sz, PROT_READ | PROT_WRITE) == 0) {
        ok++;
    }
    p[0] = 'B';
    if (p[0] == 'B') {
        ok++;
    }
    if (mprotect(p, sz, PROT_NONE) == 0) {
        ok++;
    }
    if (mprotect(p, sz, PROT_READ | PROT_WRITE) == 0) {
        ok++;
    }

    munmap(p, sz);
    printf("mprot ok=%d\n", ok);
    return 0;
}
