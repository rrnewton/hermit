#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/*
 * fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE) deallocation mode.
 * Distinct from the plain grow-mode allocation exercised by file_mutation:
 * this punches a hole in the middle of a written file and asserts the
 * backend-invariant data effect rather than any host-specific metadata:
 *   1. mkstemp opens a private temp file.
 *   2. writing 8192 bytes of 'z' succeeds.
 *   3. punching a 4096-byte hole at offset 4096 (KEEP_SIZE) returns 0.
 *   4. the logical size stays 8192 (KEEP_SIZE preserves EOF).
 *   5. the punched region reads back fully.
 *   6. every byte of the punched region reads as zero.
 * The file is removed before printing so a --verify second pass re-runs
 * against a clean filesystem with the same determinized mkstemp name.
 */
int main(void) {
    int ok = 0;
    char path[] = "/tmp/punchXXXXXX";
    int fd = mkstemp(path);
    if (fd >= 0) {
        ok++;
    }

    char buf[8192];
    memset(buf, 'z', sizeof(buf));
    if (write(fd, buf, sizeof(buf)) == (ssize_t)sizeof(buf)) {
        ok++;
    }

    if (fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 4096) == 0) {
        ok++;
    }

    struct stat st;
    if (fstat(fd, &st) == 0 && st.st_size == 8192) {
        ok++;
    }

    char rb[4096];
    if (pread(fd, rb, sizeof(rb), 4096) == (ssize_t)sizeof(rb)) {
        ok++;
    }

    int allzero = 1;
    for (size_t i = 0; i < sizeof(rb); i++) {
        if (rb[i] != 0) {
            allzero = 0;
        }
    }
    if (allzero) {
        ok++;
    }

    close(fd);
    unlink(path);
    printf("punch ok=%d\n", ok);
    return 0;
}
