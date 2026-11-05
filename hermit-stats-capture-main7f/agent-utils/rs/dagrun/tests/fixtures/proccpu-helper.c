/* Native lifecycle fixture shared by both editions; no runtime under test is emulated. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <sys/ptrace.h>
#include <sys/prctl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void burn(void) {
    struct timespec first, now;
    if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &first)) _exit(10);
    do {
        if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &now)) _exit(11);
    } while ((now.tv_sec - first.tv_sec) + (now.tv_nsec - first.tv_nsec) / 1e9 < .15);
}
static void *worker(void *unused) {
    (void)unused;
    puts("worker"); fflush(stdout);
    if (getchar() == EOF) _exit(12);
    burn();
    puts("worked"); fflush(stdout);
    (void)getchar();
    _exit(0);
}

static const char *image;
static void *exec_worker(void *unused) {
    (void)unused;
    puts("thread"); fflush(stdout);
    if (getchar() == EOF) _exit(20);
    execl(image, image, "exec-after", NULL);
    perror("execl"); _exit(21);
}
static int traced_zombie(void) {
    int ids[2], go[2], command[2], ack[2];
    if (pipe(ids) || pipe(go) || pipe(command) || pipe(ack)) return 30;
    pid_t owner = getpid();
    pid_t parent = fork();
    if (parent < 0) return 31;
    if (!parent) {
        if (prctl(PR_SET_PDEATHSIG, SIGKILL) || getppid() != owner) _exit(32);
        close(ids[0]); close(command[1]); close(ack[0]);
        pid_t real_parent = getpid();
        pid_t child = fork();
        if (child < 0) _exit(33);
        if (!child) {
            if (prctl(PR_SET_PDEATHSIG, SIGKILL) || getppid() != real_parent) _exit(34);
            close(ids[1]); close(go[1]); close(command[0]); close(ack[1]);
            char byte;
            if (read(go[0], &byte, 1) != 1) _exit(35);
            burn(); _exit(0);
        }
        close(go[0]); close(go[1]);
        if (write(ids[1], &child, sizeof(child)) != sizeof(child)) _exit(36);
        close(ids[1]);
        char byte;
        if (read(command[0], &byte, 1) != 1) _exit(37);
        if (waitpid(child, NULL, 0) != child) _exit(38);
        if (write(ack[1], "r", 1) != 1) _exit(39);
        (void)read(command[0], &byte, 1);
        _exit(0);
    }
    close(ids[1]); close(go[0]); close(command[0]); close(ack[1]);
    pid_t child;
    if (read(ids[0], &child, sizeof(child)) != sizeof(child)) return 40;
    close(ids[0]);
    if (ptrace(PTRACE_SEIZE, child, NULL, NULL)) { perror("PTRACE_SEIZE"); return 41; }
    if (write(go[1], "g", 1) != 1) return 42;
    close(go[1]);
    siginfo_t info;
    if (waitid(P_PID, child, &info, WEXITED | WNOWAIT | __WALL)) { perror("trace waitid"); return 43; }
    printf("%d\n", child); fflush(stdout);
    if (getchar() == EOF) return 44;
    /* A tracer that is not the real parent consumes EXIT_TRACE, not CPU credit. */
    if (waitpid(child, NULL, __WALL) != child) { perror("trace waitpid"); return 45; }
    puts("detached"); fflush(stdout);
    if (getchar() == EOF) return 46;
    if (write(command[1], "r", 1) != 1) return 47;
    char byte;
    if (read(ack[0], &byte, 1) != 1) return 48;
    puts("reaped"); fflush(stdout);
    if (getchar() == EOF) return 49;
    if (write(command[1], "x", 1) != 1) return 50;
    if (waitpid(parent, NULL, 0) != parent) return 51;
    return 0;
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    if (!strcmp(argv[1], "trace")) return traced_zombie();
    if (!strcmp(argv[1], "exec")) {
        image = argv[0];
        pthread_t thread;
        if (pthread_create(&thread, NULL, exec_worker, NULL)) return 22;
        for (;;) pause();
    }
    if (!strcmp(argv[1], "exec-after")) {
        puts("executed"); fflush(stdout);
        if (getchar() == EOF) return 23;
        burn();
        puts("worked"); fflush(stdout);
        (void)getchar();
        return 0;
    }
    if (!strcmp(argv[1], "leader")) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, worker, NULL)) return 3;
        pthread_exit(NULL);
    }
    if (!strcmp(argv[1], "comm") && prctl(PR_SET_NAME, "p\xff)\n(\x80")) return 52;
    if (strcmp(argv[1], "zombie") && strcmp(argv[1], "comm")) return 4;
    pid_t pid = fork();
    if (pid < 0) return 5;
    if (!pid) { burn(); _exit(0); }
    siginfo_t info;
    if (waitid(P_PID, pid, &info, WEXITED | WNOWAIT)) return 6;
    printf("%d\n", pid); fflush(stdout);
    if (getchar() == EOF) return 7;
    if (waitpid(pid, NULL, 0) != pid) return 8;
    puts("reaped"); fflush(stdout);
    (void)getchar();
    return 0;
}
