// A scheduler-sent signal that lands after a nanosleep's deadline must
// not end the sleep (Linux do_nanosleep returns 0 once the deadline has
// passed), and a default-ignored SIGCHLD must never wake a sleep. A
// timer that lands before the deadline still interrupts it.
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;

static void on_alarm(int sig) {
    (void)sig;
    alarms++;
}

static int sleep_ms(long ms) {
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (ms % 1000) * 1000000L;
    errno = 0;
    int rc = nanosleep(&ts, NULL);
    return rc == 0 ? 0 : -errno;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    if (strcmp(argv[1], "timer-after-deadline") == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = on_alarm;
        sigemptyset(&sa.sa_mask);
        if (sigaction(SIGALRM, &sa, NULL) != 0) return 3;
        struct itimerval it;
        memset(&it, 0, sizeof(it));
        it.it_value.tv_usec = 300 * 1000;
        if (setitimer(ITIMER_REAL, &it, NULL) != 0) return 4;
        int first = sleep_ms(100);
        printf("first nanosleep %d, alarms %d\n", first, (int)alarms);
        // The timer lands 200 ms into this sleep: it must interrupt it.
        int second = sleep_ms(500);
        printf("second nanosleep %d, alarms %d\n", second, (int)alarms);
        return 0;
    }
    if (strcmp(argv[1], "ignored-sigchld") == 0) {
        if (signal(SIGCHLD, SIG_IGN) == SIG_ERR) return 3;
        pid_t child = fork();
        if (child < 0) return 4;
        if (child == 0) _exit(0);
        int rc = sleep_ms(300);
        printf("nanosleep %d\n", rc);
        return 0;
    }
    return 2;
}
