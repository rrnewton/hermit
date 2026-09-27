/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the LICENSE file. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

/* Raw x86-64 Linux waitid writes these six scalars, not a whole siginfo_t.
 * Its rusage copy precedes them, including on a consuming copyout fault.
 * KVM's raw rusage bytes are zero; this is not native accounting parity. */
_Static_assert(sizeof(siginfo_t) == 128, "x86-64 siginfo ABI");
_Static_assert(sizeof(struct rusage) == 144, "x86-64 rusage ABI");
_Static_assert(offsetof(siginfo_t, si_pid) == 16, "SIGCHLD union offset");
static const size_t field_offsets[] = {0, 4, 8, 16, 20, 24};
static pid_t children[64];
static size_t child_count, page_size, cases, assertions;
static volatile sig_atomic_t alarms;

static void require(int yes, const char *what) {
  ++assertions;
  if (!yes) {
    fprintf(stderr, "waitid-copyout assertion failed: %s (errno=%d)\n", what,
            errno);
    fflush(stdout);
    exit(1);
  }
}

static void hex(const unsigned char *bytes, size_t length) {
  static const char digits[] = "0123456789abcdef";
  for (size_t i = 0; i < length; ++i) {
    putchar(digits[bytes[i] >> 4]);
    putchar(digits[bytes[i] & 15]);
  }
}

struct result { long value; int error; };

static struct result raw_waitid(int which, long id, void *info,
                                unsigned long options, void *usage) {
  errno = 0;
  long value = syscall(SYS_waitid, which, id, info, options, usage);
  return (struct result){value, errno};
}

static void fields(unsigned char *where, size_t count, pid_t child, int status) {
  int32_t values[] = {child ? SIGCHLD : 0, 0, child ? CLD_EXITED : 0,
                      child, child ? (int32_t)getuid() : 0, status};
  for (size_t i = 0; i < count; ++i)
    memcpy(where + field_offsets[i], &values[i], sizeof(values[i]));
}

/* No failed test can leave a pipe-held child behind. The official Rust
 * harness also owns the whole process group and a finite wall deadline. */
static void cleanup_children(void) {
  for (size_t i = 0; i < child_count; ++i) {
    pid_t child = children[i];
    if (child <= 0) continue;
    int status;
    pid_t found = waitpid(child, &status, WNOHANG);
    if (found == 0) {
      (void)kill(child, SIGKILL);
      (void)waitpid(child, &status, 0);
    }
  }
}

static void forget(pid_t child) {
  for (size_t i = 0; i < child_count; ++i)
    if (children[i] == child) {
      children[i] = 0;
      return;
    }
  require(0, "exact tracked child");
}

static void track(pid_t child) {
  require(child > 0 && child_count < 64, "finite fresh child");
  children[child_count++] = child;
}

static void info_check(const char *name, pid_t child, unsigned long options,
                       int error, int event_status) {
  unsigned char bytes[160], expected[160];
  memset(bytes, 0xa5, sizeof(bytes));
  memcpy(expected, bytes, sizeof(bytes));
  fields(expected + 16, 6, event_status >= 0 ? child : 0,
         event_status >= 0 ? event_status : 0);
  struct result r = raw_waitid(P_PID, child, bytes + 16, options, NULL);
  printf("{\"type\":\"check\",\"name\":\"%s\",\"pid\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"options\":\"%#lx\",\"arena\":\"",
         name, child, r.value, r.error, options);
  hex(bytes, sizeof(bytes));
  puts("\"}");
  fflush(stdout);
  require(r.value == (error ? -1 : 0) && r.error == error, name);
  require(memcmp(bytes, expected, sizeof(bytes)) == 0,
          "six fields and all stack arena guards/padding");
}

static pid_t terminal_child(int status) {
  pid_t child = fork();
  if (child == 0) _exit(status);
  track(child);
  info_check("terminal-peek", child, WEXITED | WNOWAIT, 0, status);
  return child;
}

struct arena {
  unsigned char *memory, *before, *expected;
  size_t length;
};

static struct arena arena_new(void) {
  struct arena a = {.length = 2 * page_size};
  a.memory = mmap(NULL, a.length, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  require(a.memory != MAP_FAILED, "map actual caller arena");
  a.before = malloc(a.length);
  a.expected = malloc(a.length);
  require(a.before && a.expected, "allocate exact arena snapshots");
  memset(a.memory, 0xa5, a.length);
  memcpy(a.before, a.memory, a.length);
  memcpy(a.expected, a.memory, a.length);
  return a;
}

static void arena_protect(struct arena *a, size_t cut, int protection) {
  if (protection != (PROT_READ | PROT_WRITE))
    require(mprotect(a->memory + (cut ? page_size : 0), page_size,
                     protection) == 0, "protect actual output page");
}

static void arena_restore(struct arena *a) {
  require(mprotect(a->memory, a->length, PROT_READ | PROT_WRITE) == 0,
          "restore arena only after waitid returns");
}

static void arena_drop(struct arena *a) {
  require(munmap(a->memory, a->length) == 0, "unmap output arena");
  free(a->before);
  free(a->expected);
}

#define RW (PROT_READ | PROT_WRITE)
struct event_case {
  const char *name;
  size_t info_cut, usage_cut, info_fields, usage_prefix;
  int info_protection, usage_protection, null_info, null_usage;
  int error, nowait, alias, all_children;
  unsigned long high_options;
};

static const struct event_case event_cases[] = {
    {.name="writable", .info_fields=6, .usage_prefix=144,
     .info_protection=RW, .usage_protection=RW},
    {.name="null-info", .usage_prefix=144, .null_info=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="null-usage", .info_fields=6, .null_usage=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="both-null", .null_info=1, .null_usage=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="protected-unused-tail", .info_cut=28, .info_fields=6,
     .usage_prefix=144, .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="read-only-info", .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="inaccessible-info", .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="read-only-usage", .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="inaccessible-usage", .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="usage-prefix-1", .usage_cut=1, .usage_prefix=1, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="usage-prefix-8", .usage_cut=8, .usage_prefix=8, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="usage-prefix-64", .usage_cut=64, .usage_prefix=64, .error=EFAULT,
     .info_protection=RW, .usage_protection=PROT_READ},
    {.name="first-scalar-split", .info_cut=1, .usage_prefix=144,
     .error=EFAULT, .info_protection=PROT_READ, .usage_protection=RW},
    {.name="pid-scalar-split", .info_cut=18, .info_fields=3,
     .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_NONE, .usage_protection=RW},
    {.name="status-scalar-split", .info_cut=27, .info_fields=5,
     .usage_prefix=144, .error=EFAULT,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="aliased-outputs", .info_fields=6, .usage_prefix=144, .alias=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="wnowait-writable", .info_fields=6, .usage_prefix=144, .nowait=1,
     .info_protection=RW, .usage_protection=RW},
    {.name="wnowait-info-fault", .usage_prefix=144, .error=EFAULT, .nowait=1,
     .info_protection=PROT_READ, .usage_protection=RW},
    {.name="wnowait-usage-fault", .usage_cut=8, .usage_prefix=8,
     .error=EFAULT, .nowait=1,
     .info_protection=RW, .usage_protection=PROT_NONE},
    {.name="high-option-register-bits", .info_fields=6, .usage_prefix=144,
     .high_options=1UL << 32, .info_protection=RW, .usage_protection=RW},
    {.name="p-all-ignores-id", .info_fields=6, .usage_prefix=144,
     .all_children=1, .info_protection=RW, .usage_protection=RW},
};

static void event_call(const struct event_case *c, pid_t child, int status) {
  struct arena i = arena_new(), u = arena_new();
  size_t io = c->info_cut ? page_size - c->info_cut : 64;
  size_t uo = c->usage_cut ? page_size - c->usage_cut : 64;
  struct arena *usage_arena = c->alias ? &i : &u;
  if (c->alias) io = 80;
  memset(usage_arena->expected + uo, 0, c->usage_prefix);
  fields(i.expected + io, c->info_fields, child, status);
  arena_protect(&i, c->info_cut, c->info_protection);
  arena_protect(&u, c->usage_cut, c->usage_protection);
  unsigned long options = WEXITED | c->high_options | (c->nowait ? WNOWAIT : 0);
  struct result r = raw_waitid(c->all_children ? P_ALL : P_PID,
                               c->all_children ? -1 : child,
                               c->null_info ? NULL : i.memory + io, options,
                               c->null_usage ? NULL : usage_arena->memory + uo);
  arena_restore(&i);
  arena_restore(&u);
  printf("{\"type\":\"case\",\"name\":\"%s\",\"pid\":%d,\"rc\":%ld,"
         "\"errno\":%d,\"info_offset\":%zu,\"usage_offset\":%zu,"
         "\"options\":\"%#lx\",\"alias\":%d,\"info_protection\":%d,"
         "\"usage_protection\":%d,\"null_info\":%d,\"null_usage\":%d,"
         "\"info_before\":\"", c->name, child, r.value, r.error, io, uo,
         options, c->alias, c->info_protection, c->usage_protection,
         c->null_info, c->null_usage);
  hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(r.value == (c->error ? -1 : 0) && r.error == c->error,
          "exact event return and errno");
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "full info arena: ordered scalar stores, padding, tail and canaries");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "full usage arena: exact zero prefix and untouched suffix/canaries");
  arena_drop(&i); arena_drop(&u); ++cases;
}

static void terminal_cases(void) {
  for (size_t n = 0; n < sizeof(event_cases) / sizeof(event_cases[0]); ++n) {
    const struct event_case *c = &event_cases[n];
    int status = 31 + (int)n;
    pid_t child = terminal_child(status);
    event_call(c, child, status);
    if (c->nowait) {
      /* A protected-info EFAULT alone could hide ECHILD. A successful NULL
       * WNOWAIT followed by the exact event proves the child was retained. */
      struct result r = raw_waitid(P_PID, child, NULL, WEXITED | WNOWAIT, NULL);
      printf("{\"type\":\"retained\",\"pid\":%d,\"rc\":%ld,\"errno\":%d}\n",
             child, r.value, r.error);
      require(r.value == 0 && r.error == 0, "WNOWAIT fault retained child");
      info_check("retained-reap", child, WEXITED, 0, status);
    }
    info_check("consumed-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
    forget(child);
  }
  pid_t first = terminal_child(61), sibling = terminal_child(62);
  struct event_case c = {.name="fault-preserves-sibling", .usage_prefix=144,
                         .error=EFAULT, .info_protection=PROT_READ,
                         .usage_protection=RW};
  event_call(&c, first, 61);
  info_check("selected-ECHILD", first, WEXITED | WNOHANG, ECHILD, -1);
  forget(first);
  info_check("exact-sibling-retained", sibling, WEXITED | WNOWAIT, 0, 62);
  info_check("exact-sibling-reaped", sibling, WEXITED, 0, 62);
  info_check("sibling-ECHILD", sibling, WEXITED | WNOHANG, ECHILD, -1);
  forget(sibling);
}

static void alarm_handler(int signal) {
  if (signal != SIGALRM) _exit(91);
  ++alarms;
}

static void error_call(const char *name, int which, long id,
                       unsigned long options, int info_mode,
                       int usage_protection, int error, int interrupted) {
  struct arena i = arena_new(), u = arena_new();
  if (info_mode == 0) fields(i.expected + 64, 6, 0, 0);
  if (info_mode == 2) arena_protect(&i, 0, PROT_READ);
  arena_protect(&u, 0, usage_protection);
  if (interrupted) {
    struct itimerval timer = {.it_value = {.tv_usec = 50000}};
    require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "arm interrupt timer");
  }
  struct result r = raw_waitid(which, id, info_mode == 1 ? NULL : i.memory + 64,
                               options, u.memory + 64);
  if (interrupted) {
    struct itimerval timer = {0};
    require(setitimer(ITIMER_REAL, &timer, NULL) == 0, "disarm interrupt timer");
    require(alarms == 1, "one actual non-restarting wait interruption");
  }
  arena_restore(&i); arena_restore(&u);
  printf("{\"type\":\"case\",\"name\":\"%s\",\"which\":%d,\"id\":%ld,"
         "\"rc\":%ld,\"errno\":%d,\"options\":\"%#lx\",\"alarms\":%d,"
         "\"info_mode\":%d,\"usage_protection\":%d,\"info_before\":\"",
         name, which, id, r.value, r.error, options, alarms,
         info_mode, usage_protection);
  hex(i.before, i.length);
  printf("\",\"info_after\":\""); hex(i.memory, i.length);
  printf("\",\"aux_before\":\""); hex(u.before, u.length);
  printf("\",\"aux_after\":\""); hex(u.memory, u.length);
  puts("\"}"); fflush(stdout);
  require(r.value == (error ? -1 : 0) && r.error == error,
          "error/no-event errno including copyout precedence");
  require(memcmp(i.memory, i.expected, i.length) == 0,
          "error/no-event writes only the six zero fields");
  require(memcmp(u.memory, u.expected, u.length) == 0,
          "no positive event must not write rusage");
  arena_drop(&i); arena_drop(&u); ++cases;
}

static void error_cases(void) {
  pid_t gone = terminal_child(71);
  info_check("setup-reap", gone, WEXITED, 0, 71); forget(gone);
  error_call("already-reaped", P_PID, gone, WEXITED, 0, RW, ECHILD, 0);
  int pipefd[2]; require(pipe(pipefd) == 0, "live-child pipe");
  pid_t child = fork();
  if (child == 0) {
    close(pipefd[1]);
    char byte;
    if (read(pipefd[0], &byte, 1) != 1) _exit(93);
    close(pipefd[0]); _exit(72);
  }
  track(child); require(close(pipefd[0]) == 0, "close parent read end");
  error_call("invalid-options", P_PID, child, WEXITED | 0x10000000UL,
             0, RW, EINVAL, 0);
  error_call("invalid-options-null", P_PID, child, 0, 1, RW, EINVAL, 0);
  error_call("invalid-options-protected", P_PID, child, WEXITED | 0x10000000UL,
             2, RW, EFAULT, 0);
  error_call("pid-zero", P_PID, 0, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("pid-zero-null", P_PID, 0, WEXITED | WNOHANG, 1, RW, EINVAL, 0);
  error_call("pid-zero-protected", P_PID, 0, WEXITED | WNOHANG,
             2, RW, EFAULT, 0);
  error_call("pid-negative", P_PID, -1, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("pgid-negative", P_PGID, -1, WEXITED | WNOHANG, 0, RW, EINVAL, 0);
  error_call("invalid-selector", 99, child, WEXITED | WNOHANG,
             0, PROT_NONE, EINVAL, 0);
  error_call("live-no-event", P_PID, child, WEXITED | WNOHANG, 0, RW, 0, 0);
  error_call("live-no-event-null", P_PID, child, WEXITED | WNOHANG, 1, RW, 0, 0);
  error_call("live-no-event-protected", P_PID, child, WEXITED | WNOHANG,
             2, RW, EFAULT, 0);
  error_call("live-usage-protected", P_PID, child, WEXITED | WNOHANG,
             0, PROT_READ, 0, 0);
  error_call("live-high-options", P_PID, child, WEXITED | WNOHANG | (1UL << 32),
             0, RW, 0, 0);
  error_call("live-p-all-ignores-id", P_ALL, -1, WEXITED | WNOHANG,
             0, RW, 0, 0);
  struct sigaction action = {.sa_handler = alarm_handler};
  sigemptyset(&action.sa_mask);
  require(sigaction(SIGALRM, &action, NULL) == 0, "non-SA_RESTART handler");
  sigset_t unblocked;
  sigemptyset(&unblocked);
  sigaddset(&unblocked, SIGALRM);
  require(sigprocmask(SIG_UNBLOCK, &unblocked, NULL) == 0,
          "unblock the process timer signal");
  error_call("interrupted-protected-info", P_PID, child, WEXITED,
             2, RW, EFAULT, 1);
  require(write(pipefd[1], "x", 1) == 1, "release exact live child");
  require(close(pipefd[1]) == 0, "close release pipe");
  info_check("live-child-reap", child, WEXITED, 0, 72);
  info_check("live-child-ECHILD", child, WEXITED | WNOHANG, ECHILD, -1);
  forget(child);
}

int main(int argc, char **argv) {
  static char stdout_buffer[65536];
  require(setvbuf(stdout, stdout_buffer, _IOFBF, sizeof(stdout_buffer)) == 0,
          "bounded buffered stdout");
  require(argc == 2, "one explicit fixture mode");
  long page = sysconf(_SC_PAGESIZE);
  require(page > 128 && page <= 65536, "finite Linux page size");
  page_size = (size_t)page;
  require(atexit(cleanup_children) == 0, "install child cleanup");
  if (strcmp(argv[1], "terminal") == 0) terminal_cases();
  else if (strcmp(argv[1], "errors") == 0) error_cases();
  else require(0, "known fixture mode");
  for (size_t i = 0; i < child_count; ++i)
    require(children[i] == 0, "every exact child retired");
  printf("{\"type\":\"summary\",\"mode\":\"%s\",\"cases\":%zu,"
         "\"children\":%zu,\"assertions\":%zu,\"passed\":true}\n",
         argv[1], cases, child_count, assertions);
  require(fflush(stdout) == 0, "retain complete output");
  return 0;
}
