// Cross-backend parity contract: copy_file_range(2) deterministic refusal.
//
// Hermit does not implement copy_file_range; every backend returns a
// deterministic ENOSYS rather than forwarding the call to the host kernel,
// where it succeeds. This fixture pins that refusal: the same errno, no
// partial progress in the kernel-managed offsets, and an untouched
// destination, repeated across calls and argument shapes. Outside Hermit the
// same syscall copies the payload (verified separately), so the row is a
// genuine determinization refusal, not a universally invalid request.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    char tmpl_src[] = "/tmp/cfr_ref_src_XXXXXX";
    char tmpl_dst[] = "/tmp/cfr_ref_dst_XXXXXX";
    int fd_src = mkstemp(tmpl_src);
    int fd_dst = mkstemp(tmpl_dst);
    if (fd_src < 0 || fd_dst < 0) {
        printf("copy_file_range refused ENOSYS ok=0 [mkstemp fail]\n");
        return 0;
    }

    const char payload[] = "copy-file-range-refusal-payload-0123456789";
    size_t n = sizeof(payload) - 1;
    if (write(fd_src, payload, n) == (ssize_t)n) ok++;   // (1) source populated
    lseek(fd_src, 0, SEEK_SET);

    // (2) full copy is refused with deterministic ENOSYS.
    off_t off_in = 0, off_out = 0;
    errno = 0;
    ssize_t c1 = copy_file_range(fd_src, &off_in, fd_dst, &off_out, n, 0);
    if (c1 == -1 && errno == ENOSYS) ok++;

    // (3) kernel-managed offsets made no partial progress.
    if (off_in == 0 && off_out == 0) ok++;

    // (4) destination remains empty; nothing was copied.
    struct stat st;
    if (fstat(fd_dst, &st) == 0 && st.st_size == 0) ok++;

    // (5) a second call with explicit non-zero offsets refuses identically.
    off_t in2 = 10, out2 = 0;
    errno = 0;
    ssize_t c2 = copy_file_range(fd_src, &in2, fd_dst, &out2, 8, 0);
    if (c2 == -1 && errno == ENOSYS && in2 == 10 && out2 == 0) ok++;

    // (6) NULL offsets (implicit file positions) are refused identically.
    errno = 0;
    ssize_t c3 = copy_file_range(fd_src, NULL, fd_dst, NULL, n, 0);
    if (c3 == -1 && errno == ENOSYS) ok++;

    close(fd_src);
    close(fd_dst);
    unlink(tmpl_src);
    unlink(tmpl_dst);
    printf("copy_file_range refused ENOSYS ok=%d\n", ok);
    return 0;
}
