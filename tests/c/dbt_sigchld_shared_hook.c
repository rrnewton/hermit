/* A real child-exit SIGCHLD must reach Detcore before its guest handler.
 * No polling, timers, artificial signal sends, or prescribed EOF ordering. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static const char handler_marker[] = "caught-sigchld\n";
static const char completed_marker[] = "completed-sigchld\n";

static void caught_sigchld(int signal) {
  const int saved_errno = errno;
  if (signal != SIGCHLD || handled != 0)
    _exit(81);
  handled = 1;
  /* The handler's first syscall is a unique, real write. Only this handler
   * writes this 15-byte marker, making the retained Detcore record joinable
   * to the actual guest output without a test-only runtime callback. */
  if (write(STDOUT_FILENO, handler_marker, sizeof(handler_marker) - 1) !=
      (ssize_t)(sizeof(handler_marker) - 1))
    _exit(82);
  sigset_t during;
  if (sigprocmask(SIG_SETMASK, NULL, &during) != 0 ||
      sigismember(&during, SIGCHLD) != 1 ||
      sigismember(&during, SIGUSR1) != 1)
    _exit(83);
  errno = saved_errno;
}

int main(void) {
  sigset_t base, child_signal;
  if (sigemptyset(&base) != 0 || sigaddset(&base, SIGUSR1) != 0 ||
      sigprocmask(SIG_SETMASK, &base, NULL) != 0 ||
      sigemptyset(&child_signal) != 0 ||
      sigaddset(&child_signal, SIGCHLD) != 0 ||
      sigprocmask(SIG_BLOCK, &child_signal, NULL) != 0)
    return 71;

  struct sigaction action = {0};
  action.sa_handler = caught_sigchld;
  if (sigemptyset(&action.sa_mask) != 0 ||
      sigaction(SIGCHLD, &action, NULL) != 0)
    return 72;
  const pid_t child = fork();
  if (child < 0)
    return 73;
  if (child == 0)
    _exit(37);

  int status = 0;
  pid_t reaped;
  do {
    reaped = waitpid(child, &status, 0);
  } while (reaped < 0 && errno == EINTR);
  if (reaped != child || !WIFEXITED(status) || WEXITSTATUS(status) != 37 ||
      handled != 0)
    return 74;
  sigset_t pending;
  if (sigpending(&pending) != 0 || sigismember(&pending, SIGCHLD) != 1)
    return 75;
  /* Linux must deliver the already pending, now unblocked signal before
   * this returns. Reaping the child must not erase a pending notification. */
  if (sigprocmask(SIG_UNBLOCK, &child_signal, NULL) != 0 || handled != 1)
    return 76;
  sigset_t after;
  if (sigprocmask(SIG_SETMASK, NULL, &after) != 0 ||
      sigismember(&after, SIGCHLD) != 0 ||
      sigismember(&after, SIGUSR1) != 1)
    return 77;
  if (write(STDOUT_FILENO, completed_marker, sizeof(completed_marker) - 1) !=
      (ssize_t)(sizeof(completed_marker) - 1))
    return 78;
  return 0;
}
