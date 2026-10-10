/* A pselect6 with more than 64 descriptors and a temporary mask, made on a
 * stack with no writable memory below its red zone (Codex re-check of
 * https://github.com/rrnewton/hermit/pull/4053).
 *
 * Usage: pselect_tight_stack <tight|adjacent|ample>
 *
 * The call runs on a stack placed 144 (tight), 160 (adjacent) or 1024 (ample)
 * bytes into a writable page with an inaccessible page below it. Each mode
 * makes four calls in turn, so the later ones reuse whatever storage the
 * earlier ones left:
 *
 *   ready    a readable pipe; returns 1.
 *   signal   SIGALRM, blocked by the thread's own mask and unblocked by the
 *            temporary one, arrives during the wait; returns -EINTR with the
 *            handler run once, on an alternate stack.
 *   restart  SIGCONT, blocked by the own mask and left to its default action,
 *            is pending when the call starts; the kernel takes it, runs no
 *            handler and restarts the call, which reads its temporary mask
 *            again, then times out after 50 ms and returns 0.
 *   again    the ready call once more.
 *
 * Natively every mode prints
 *   mode=<mode> ready=1 signal=-4 alarms=1 restart=0 again=1 oracle=1
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__) || !defined(SYS_pselect6) || SYS_pselect6 != 270
#error This fixture specifies the Linux x86_64 pselect6 syscall ABI.
#endif

/* No instruction between the stack switch and its restoration touches the
 * stack. SysV inputs: new_sp, nfds, readfds, timeout, wrapper. Preserves the
 * caller's r12 on the original stack and keeps the original rsp in r12 across
 * the syscall. */
extern long pselect_on_stack(void *, long, fd_set *, struct timespec *, void *);
__asm__(
    ".text\n"
    ".globl pselect_on_stack\n"
    ".type pselect_on_stack,@function\n"
    "pselect_on_stack:\n"
    "push %r12\n"
    "mov %rsp,%r12\n"
    "mov %rdi,%r11\n"
    "mov %rsi,%rdi\n"
    "mov %rdx,%rsi\n"
    "mov %rcx,%rax\n"
    "xor %edx,%edx\n"
    "xor %r10d,%r10d\n"
    "mov %r8,%r9\n"
    "mov %rax,%r8\n"
    "mov %r11,%rsp\n"
    "mov $270,%eax\n"
    "syscall\n"
    "mov %r12,%rsp\n"
    "pop %r12\n"
    "ret\n"
    ".size pselect_on_stack,.-pselect_on_stack\n");

#define NFDS 65

static uint64_t empty_mask;
static struct {
    const uint64_t *mask;
    size_t bytes;
} wrapper = {&empty_mask, sizeof(empty_mask)};
static volatile sig_atomic_t alarms;
static unsigned char alternate[65536];

static void on_alarm(int signo) {
    (void)signo;
    alarms++;
}

static long select_on(unsigned char *stack, int fd, long seconds, long nanos) {
    fd_set readable;
    struct timespec timeout = {seconds, nanos};
    FD_ZERO(&readable);
    FD_SET(fd, &readable);
    return pselect_on_stack(stack, NFDS, &readable, &timeout, &wrapper);
}

int main(int argc, char **argv) {
    size_t offset;
    if (argc != 2)
        return 2;
    if (!strcmp(argv[1], "tight"))
        offset = 144; /* 16 bytes below rsp minus the 128-byte red zone */
    else if (!strcmp(argv[1], "adjacent"))
        offset = 160;
    else if (!strcmp(argv[1], "ample"))
        offset = 1024;
    else
        return 2;
    long page_long = sysconf(_SC_PAGESIZE);
    if (page_long <= 0 || (size_t)page_long <= offset)
        return 3;
    size_t page = (size_t)page_long;
    unsigned char *area =
        mmap(NULL, page * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (area == MAP_FAILED)
        return 4;
    unsigned char *writable = area + page;
    if (mprotect(writable, page, PROT_READ | PROT_WRITE) != 0)
        return 5;
    unsigned char *stack = writable + offset;

    stack_t alt = {.ss_sp = alternate, .ss_size = sizeof(alternate), .ss_flags = 0};
    if (sigaltstack(&alt, NULL) != 0)
        return 6;
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_alarm;
    action.sa_flags = SA_ONSTACK;
    if (sigaction(SIGALRM, &action, NULL) != 0)
        return 7;
    sigset_t own;
    sigemptyset(&own);
    sigaddset(&own, SIGALRM);
    sigaddset(&own, SIGCONT);
    if (sigprocmask(SIG_SETMASK, &own, NULL) != 0)
        return 8;

    int ready_pipe[2], idle_pipe[2];
    if (pipe(ready_pipe) != 0 || pipe(idle_pipe) != 0 || ready_pipe[0] >= 64 ||
        idle_pipe[0] >= 64)
        return 9;
    if (write(ready_pipe[1], "x", 1) != 1)
        return 10;

    long ready = select_on(stack, ready_pipe[0], 2, 0);

    struct itimerval timer = {{0, 0}, {0, 100000}};
    if (setitimer(ITIMER_REAL, &timer, NULL) != 0)
        return 11;
    long signal = select_on(stack, idle_pipe[0], 2, 0);
    int alarms_seen = alarms;

    if (kill(getpid(), SIGCONT) != 0)
        return 12;
    long restart = select_on(stack, idle_pipe[0], 0, 50000000);

    long again = select_on(stack, ready_pipe[0], 2, 0);

    int oracle = ready == 1 && signal == -EINTR && alarms_seen == 1 && restart == 0 &&
                 again == 1;
    printf("mode=%s ready=%ld signal=%ld alarms=%d restart=%ld again=%ld oracle=%d\n",
           argv[1], ready, signal, alarms_seen, restart, again, oracle);
    return oracle ? 0 : 42;
}
