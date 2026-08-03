#include <stdio.h>
#include <sys/personality.h>

/*
 * Process execution-domain round-trip via personality(2). It asserts only
 * backend-invariant relational properties -- never the absolute host default
 * persona -- so the golden stdout is portable across hosts and backends:
 *   1. querying the current persona (argument 0xffffffff) succeeds.
 *   2. setting the ADDR_NO_RANDOMIZE flag on top of it succeeds.
 *   3. a re-query reflects the ADDR_NO_RANDOMIZE flag.
 *   4. restoring the original persona succeeds.
 *   5. a final query echoes the exact original persona (the round-trip).
 *
 * personality() returns (unsigned int)-1 on error. The starting persona value
 * is captured at runtime and only compared to itself, so a host-specific
 * default cannot leak into the golden output; only the relational count does.
 */
int main(void) {
    int ok = 0;
    int rc = personality(0xffffffff);
    unsigned int start = (unsigned int)rc;
    if (rc != -1) {
        ok++;
    }
    if (personality(start | ADDR_NO_RANDOMIZE) != -1) {
        ok++;
    }
    rc = personality(0xffffffff);
    if (rc != -1 && ((unsigned int)rc & ADDR_NO_RANDOMIZE)) {
        ok++;
    }
    if (personality(start) != -1) {
        ok++;
    }
    rc = personality(0xffffffff);
    if (rc != -1 && (unsigned int)rc == start) {
        ok++;
    }
    printf("pers ok=%d\n", ok);
    return 0;
}
