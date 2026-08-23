// Cross-backend parity contract: shutdown(2) on an AF_UNIX socket pair.
//
// Half-closes and fully closes connected socket-pair endpoints and checks the
// return value of each shutdown. shutdown returns immediately and transfers no
// data, so there is no blocking cross-endpoint wait for the scheduler to order
// (a blocking read would livelock the DBT cooperative scheduler). No byte is
// ever written after a shutdown, so no SIGPIPE is raised — signal delivery is
// deliberately avoided, keeping this a pure return-value contract.
//
// Every outcome asserted here is a property of the guest's own socket lifecycle
// (a connected pair the guest just created, or the invalid descriptor -1), so
// the answer carries no host state and is identical across hosts.
#include <errno.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

/* Number of behavioural checks this fixture must complete; a lower count is a
   failure, not a smaller success. */
#define EXPECTED_CHECKS 5

int main(void) {
    int ok = 0;
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("shutdown ok=0 [stream socketpair fail]\n");
        return 1;
    }

    if (shutdown(sv[0], SHUT_RD) == 0) ok++;    // (1) half-close read side.
    if (shutdown(sv[0], SHUT_WR) == 0) ok++;    // (2) half-close write side.
    if (shutdown(sv[1], SHUT_RDWR) == 0) ok++;  // (3) full-close the other end.
    close(sv[0]);
    close(sv[1]);

    int dv[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, dv) == 0) {
        if (shutdown(dv[0], SHUT_RDWR) == 0) ok++;  // (4) datagram-pair endpoint.
        close(dv[0]);
        close(dv[1]);
    }

    // (5) shutdown on an invalid descriptor fails deterministically with EBADF.
    if (shutdown(-1, SHUT_RDWR) == -1 && errno == EBADF) ok++;

    printf("shutdown ok=%d\n", ok);
    /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a regression only lowered the printed
       number -- and under --verify both runs lower it identically, so the
       comparison still matches and the cell stays green. Every check above is
       unchanged; this only requires all of them. */
    if (ok != EXPECTED_CHECKS) {
        fprintf(stderr, "shutdown completed %d of %d checks\n", ok, EXPECTED_CHECKS);
        return 1;
    }
    return 0;
}
