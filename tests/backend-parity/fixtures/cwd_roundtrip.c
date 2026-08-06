#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/*
 * Process working-directory round-trip via chdir/getcwd/fchdir. It asserts only
 * backend-invariant relational properties -- never an absolute host path -- so
 * the golden stdout is portable across hosts and backends:
 *   1. getcwd captures the starting directory.
 *   2. chdir into a fresh mkdtemp directory succeeds.
 *   3. after chdir, getcwd differs from the start (the process actually moved).
 *   4. fchdir back through a directory fd opened on the start restores it.
 *   5. getcwd now equals the saved start again (the fchdir round-trip).
 *   6. chdir back to the start by absolute path also restores it.
 *
 * The starting path is never printed, so a container- or host-specific working
 * directory cannot leak into the golden output; only the relational count does.
 */
int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;
    char start[4096];
    if (getcwd(start, sizeof(start)) == NULL) {
        printf("cwd ok=0\n");
        return EXIT_FAILURE;
    }
    ok++;
    int startfd = open(start, O_RDONLY | O_DIRECTORY);

    char tmpl[] = "/tmp/cwd_roundtrip_XXXXXX";
    char *dir = mkdtemp(tmpl);
    if (dir != NULL && chdir(dir) == 0) {
        ok++;
    }
    char moved[4096];
    int moved_differs = (getcwd(moved, sizeof(moved)) != NULL && strcmp(moved, start) != 0);
    if (moved_differs) {
        ok++;
    }
    if (startfd >= 0 && fchdir(startfd) == 0) {
        ok++;
    }
    char back1[4096];
    int back1_eq = (getcwd(back1, sizeof(back1)) != NULL && strcmp(back1, start) == 0);
    if (back1_eq) {
        ok++;
    }
    /* leave via a second chdir to confirm absolute-path chdir round-trips too */
    int abs_roundtrip = 0;
    if (dir != NULL && chdir(dir) == 0 && chdir(start) == 0) {
        char back2[4096];
        abs_roundtrip = (getcwd(back2, sizeof(back2)) != NULL && strcmp(back2, start) == 0);
        if (abs_roundtrip) {
            ok++;
        }
    }
    if (startfd >= 0) {
        close(startfd);
    }
    if (dir != NULL) {
        rmdir(dir);
    }
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    /* De-ALIAS the sum. A cwd path is inherently host-dependent, so unlike the other
      * fixtures there is no host-independent VALUE to print here -- but `ok=%d` is a
      * SUM, and two backends that fail DIFFERENT checks produce the SAME total and
      * compare equal. Naming each outcome removes that aliasing. */
    printf("cwd ok=%d moved_differs=%d back1_eq=%d abs_roundtrip=%d\n",
           ok, moved_differs, back1_eq, abs_roundtrip);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
