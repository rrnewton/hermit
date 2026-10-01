/* Test-only same-process stdio adapter. Run only through bin/safehermit in an
 * independently admitted bounded harness. It never forks or drains the pipe.
 * Exit 122 is adapter failure; only the exec'd private CLI may return 125. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

extern char **environ;

static void fail(void) { _exit(122); }

int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[1], "full") && strcmp(argv[1], "normal")))
        fail();
    int executable = open(argv[2], O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    struct stat st;
    unsigned char magic[4];
    if (executable < 0 || fstat(executable, &st) || !S_ISREG(st.st_mode) ||
        pread(executable, magic, sizeof(magic), 0) != sizeof(magic) ||
        memcmp(magic, "\177ELF", sizeof(magic)))
        fail();

    int capacity = 0, filled = 0;
    if (!strcmp(argv[1], "full")) {
        int ends[2];
        if (pipe2(ends, O_CLOEXEC | O_NONBLOCK))
            fail();
        capacity = fcntl(ends[1], F_GETPIPE_SZ);
        if (capacity <= 0 || capacity > 1024 * 1024)
            fail();
        char bytes[4096];
        memset(bytes, 'x', sizeof(bytes));
        for (;;) {
            ssize_t n = write(ends[1], bytes, sizeof(bytes));
            if (n > 0) {
                filled += (int)n;
                if (filled > capacity)
                    fail();
            } else if (n < 0 && errno == EAGAIN) {
                break;
            } else {
                fail();
            }
        }
        int queued = -1;
        if (ioctl(ends[0], FIONREAD, &queued) || queued != capacity ||
            filled != capacity)
            fail();
        /* Only our new write OFD changes flags. The inherited stderr OFD is
         * never changed. Keep the unread read end alive through real exec. */
        int flags = fcntl(ends[1], F_GETFL);
        if (flags < 0 || fcntl(ends[1], F_SETFL, flags & ~O_NONBLOCK) ||
            fcntl(ends[0], F_SETFD, 0) || dup2(ends[1], STDERR_FILENO) < 0 ||
            close(ends[1]) || (fcntl(STDERR_FILENO, F_GETFL) & O_NONBLOCK))
            fail();
    }
    /* Fixture evidence only, on the separately drained stdout stream. The
     * record is not a substitute for natural CLI exit and physical cleanup. */
    char witness[160];
    int n = snprintf(witness, sizeof(witness),
                     "startup-stderr-fixture mode=%s capacity=%d queued=%d\n",
                     argv[1], capacity, filled);
    if (n <= 0 || (size_t)n >= sizeof(witness) ||
        write(STDOUT_FILENO, witness, (size_t)n) != n)
        fail();
    /* Malformed private arguments refuse before descriptor census or provider
     * creation. No guest, service, BPF object, or provider input is admitted. */
    char *const child[] = {argv[2], "--accepted-private-stdin-v1", NULL};
    fexecve(executable, child, environ);
    fail();
}
