#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/*
 * mremap(2) resize with data preservation. Distinct from the mmap_determinism
 * layout rows, which compare address sequences: this asserts only the
 * backend-invariant data effect of growing and shrinking a mapping, never any
 * address, so a moved mapping (MREMAP_MAYMOVE) is fine:
 *   1. a one-page private anonymous mapping is created and filled with 'Q'.
 *   2. mremap grows it to two pages (MREMAP_MAYMOVE) without failing.
 *   3. the original page's 'Q' bytes survive the resize (and possible move).
 *   4. the newly added second page is writable and reads back 'R'.
 *   5. mremap shrinks it back to one page with the first page's 'Q' intact.
 *   6. munmap releases the final mapping.
 * No file or address is inspected, so the golden output is portable across
 * backend-local memory layouts.
 */
int main(void) {
    int ok = 0;
    long pg = sysconf(_SC_PAGESIZE);

    char *p = mmap(NULL, (size_t)pg, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p != MAP_FAILED) {
        ok++;
    }
    memset(p, 'Q', (size_t)pg);

    char *g = mremap(p, (size_t)pg, (size_t)pg * 2, MREMAP_MAYMOVE);
    if (g != MAP_FAILED) {
        ok++;
    }

    int keep = 1;
    for (long i = 0; i < pg; i++) {
        if (g[i] != 'Q') {
            keep = 0;
        }
    }
    if (keep) {
        ok++;
    }

    memset(g + pg, 'R', (size_t)pg);
    if (g[pg] == 'R' && g[pg * 2 - 1] == 'R') {
        ok++;
    }

    char *s = mremap(g, (size_t)pg * 2, (size_t)pg, 0);
    if (s != MAP_FAILED && s[0] == 'Q' && s[pg - 1] == 'Q') {
        ok++;
    }

    if (munmap(s, (size_t)pg) == 0) {
        ok++;
    }

    printf("mremap ok=%d\n", ok);
    return 0;
}
