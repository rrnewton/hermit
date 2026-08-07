/*
 * qemu_pipe_init.c — M11: parent<->child pipe IPC as PID 1 under a guest kernel.
 *
 * M10 proved fork/execve/wait4 determinism in the guest. This adds the IPC leg:
 * PID 1 creates a pipe, forks, the child WRITES a known payload and exits with
 * an exact status, and the parent READS it back, asserts the bytes byte-for-byte,
 * then wait4()s for that exact status. Every step emits a marker, so a run that
 * boots but never exercises the pipe is distinguishable from one that did — a
 * run with zero lifecycle engagement is a NO_RESULT, not a pass and not a fail.
 *
 * Freestanding and static on purpose: no libc, raw syscalls only, so the guest
 * has no dynamic loader, no allocator and no libc buffering between the syscall
 * and the observable. Buffering matters here specifically — a libc-buffered
 * write would make the pipe transfer invisible in the log ordering that
 * --verify compares.
 *
 * Determinism argument for the payload check: the child writes a FIXED byte
 * string of known length, and the parent loops read() to EOF and compares
 * length and content exactly. Nothing here reads a clock, a pid, an address or
 * any host state, so the emitted markers are a pure function of the fixture.
 */

enum {
  SYS_READ = 0,
  SYS_WRITE = 1,
  SYS_CLOSE = 3,
  SYS_EXIT = 60,
  SYS_WAIT4 = 61,
  SYS_FORK = 57,
  SYS_UNAME = 63,
  SYS_SYNC = 162,
  SYS_REBOOT = 169,
  SYS_PIPE2 = 293,
  STDOUT_FILENO = 1,
};

enum {
  REBOOT_MAGIC1 = 0xfee1dead,
  REBOOT_MAGIC2 = 0x28121969,
  REBOOT_CMD_POWER_OFF = 0x4321fedc,
};

/* The exact contract between the two processes. Both sides are compiled from
 * these same constants, so a mismatch is a real IPC failure, not a skew. */
#define PIPE_PAYLOAD "M11_PIPE_PAYLOAD:the-quick-brown-fox-0123456789"
/* DERIVED, never hand-counted. A hardcoded length here was wrong by one on the
 * first build (46 vs the real 47), which would have surfaced as exact=0 and read
 * as an IPC failure rather than as an arithmetic mistake in the oracle. */
#define PIPE_PAYLOAD_LEN ((long)(sizeof(PIPE_PAYLOAD) - 1))
#define CHILD_EXIT_STATUS 9

struct utsname {
  char sysname[65];
  char nodename[65];
  char release[65];
  char version[65];
  char machine[65];
  char domainname[65];
};

static long syscall1(long n, long a1) {
  long r;
  __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
  return r;
}

static long syscall2(long n, long a1, long a2) {
  long r;
  __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2)
                   : "rcx", "r11", "memory");
  return r;
}

static long syscall0(long n) {
  long r;
  __asm__ volatile("syscall" : "=a"(r) : "a"(n) : "rcx", "r11", "memory");
  return r;
}

static long syscall3(long n, long a1, long a2, long a3) {
  long r;
  __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2), "d"(a3)
                   : "rcx", "r11", "memory");
  return r;
}

static long syscall4(long n, long a1, long a2, long a3, long a4) {
  long r;
  register long r10 __asm__("r10") = a4;
  __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10)
                   : "rcx", "r11", "memory");
  return r;
}

static unsigned long slen(const char *s) {
  unsigned long n = 0;
  while (s[n]) ++n;
  return n;
}

static void out(const char *s) {
  syscall3(SYS_WRITE, STDOUT_FILENO, (long)s, (long)slen(s));
}

static void outn(const char *s, long n) {
  syscall3(SYS_WRITE, STDOUT_FILENO, (long)s, n);
}

static void put_dec(char *buf, int *pos, long v) {
  char tmp[24];
  int n = 0;
  if (v == 0) {
    buf[(*pos)++] = '0';
    return;
  }
  if (v < 0) {
    buf[(*pos)++] = '-';
    v = -v;
  }
  while (v > 0 && n < 24) {
    tmp[n++] = (char)('0' + (v % 10));
    v /= 10;
  }
  while (n > 0) buf[(*pos)++] = tmp[--n];
}

static void power_off(void) {
  syscall0(SYS_SYNC);
  syscall4(SYS_REBOOT, REBOOT_MAGIC1, REBOOT_MAGIC2, REBOOT_CMD_POWER_OFF, 0);
  for (;;) syscall0(SYS_SYNC);
}

void _start(void) {
  char line[256];
  int pos;

  struct utsname sys;
  if (syscall1(SYS_UNAME, (long)&sys) < 0) {
    out("SHARED_FUTEX_QEMU_UNAME_FAILED\n");
    syscall1(SYS_EXIT, 1);
  }
  out("SHARED_FUTEX_QEMU_KERNEL_OK release=");
  out(sys.release);
  out(" machine=");
  out(sys.machine);
  out("\n");

  int fds[2] = {-1, -1};
  if (syscall2(SYS_PIPE2, (long)fds, 0) < 0) {
    out("QEMU_PIPE_CREATE_FAILED\n");
    power_off();
  }
  out("QEMU_PIPE_LAUNCH payload_len=");
  pos = 0;
  put_dec(line, &pos, PIPE_PAYLOAD_LEN);
  line[pos++] = '\n';
  outn(line, pos);

  long pid = syscall0(SYS_FORK);
  if (pid == 0) {
    /* child: writer. Close the read end first so the parent sees EOF. */
    syscall1(SYS_CLOSE, fds[0]);
    const char *msg = PIPE_PAYLOAD;
    long left = PIPE_PAYLOAD_LEN;
    long off = 0;
    while (left > 0) {
      long w = syscall3(SYS_WRITE, fds[1], (long)(msg + off), left);
      if (w <= 0) {
        out("QEMU_PIPE_CHILD_WRITE_FAILED\n");
        syscall1(SYS_EXIT, 126);
      }
      off += w;
      left -= w;
    }
    syscall1(SYS_CLOSE, fds[1]);
    syscall1(SYS_EXIT, CHILD_EXIT_STATUS);
  }
  if (pid < 0) {
    out("QEMU_PIPE_FORK_FAILED\n");
    power_off();
  }

  /* parent: reader. Close the write end or read() never sees EOF. */
  syscall1(SYS_CLOSE, fds[1]);
  char got[128];
  long total = 0;
  for (;;) {
    long r = syscall3(SYS_READ, fds[0], (long)(got + total),
                      (long)(int)(sizeof(got) - (unsigned long)total));
    if (r < 0) {
      out("QEMU_PIPE_READ_FAILED\n");
      power_off();
    }
    if (r == 0) break; /* EOF: the child closed its write end */
    total += r;
    if (total >= (long)sizeof(got)) break;
  }
  syscall1(SYS_CLOSE, fds[0]);

  int match = (total == PIPE_PAYLOAD_LEN);
  if (match) {
    const char *want = PIPE_PAYLOAD;
    for (long i = 0; i < PIPE_PAYLOAD_LEN; ++i) {
      if (got[i] != want[i]) {
        match = 0;
        break;
      }
    }
  }
  pos = 0;
  {
    const char *p = "QEMU_PIPE_PAYLOAD bytes=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
  }
  put_dec(line, &pos, total);
  {
    const char *p = " exact=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
  }
  put_dec(line, &pos, match ? 1 : 0);
  line[pos++] = '\n';
  outn(line, pos);

  long wstatus = 0;
  long r = syscall4(SYS_WAIT4, pid, (long)&wstatus, 0, 0);
  if (r < 0) {
    out("QEMU_PIPE_WAIT_FAILED\n");
    power_off();
  }
  int exited = (wstatus & 0x7f) == 0;
  int code = (int)((wstatus >> 8) & 0xff);
  int sig = (int)(wstatus & 0x7f);

  pos = 0;
  {
    const char *p = "QEMU_PIPE_EXIT ";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
  }
  if (exited) {
    const char *p = "exited=1 status=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
    put_dec(line, &pos, code);
  } else {
    const char *p = "exited=0 signal=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
    put_dec(line, &pos, sig);
  }
  line[pos++] = '\n';
  outn(line, pos);

  out("QEMU_PIPE_DONE\n");
  power_off();
}
