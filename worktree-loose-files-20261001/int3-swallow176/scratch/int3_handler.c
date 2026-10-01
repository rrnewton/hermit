/* In-process SIGTRAP handler: does the guest's own handler observe its int3? */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static volatile sig_atomic_t hits = 0;
static volatile sig_atomic_t last_code = -12345;
static void on_trap(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)uc;
    hits++;
    last_code = si->si_code;
}
int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_trap;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGTRAP, &sa, NULL) != 0) { perror("sigaction"); return 2; }
    __asm__ __volatile__("int3");
    printf("handler_hits=%d si_code=%d\n", (int)hits, (int)last_code);
    return hits ? 0 : 1;
}
