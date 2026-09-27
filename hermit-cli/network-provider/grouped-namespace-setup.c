/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */
#define _GNU_SOURCE
#include "grouped-namespace-policy.h"
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/nsfs.h>
#include <poll.h>
#include <sched.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SO_PEERPIDFD
#define SO_PEERPIDFD 77
#endif

/* Trusted installation code, like the manager setup it replaces. A supplied
 * digest is NOT permission to execute setup code with these capabilities.
 * The controller owns the authenticated SourceTerminal/namespace before it
 * launches this fixed program. This program never opens a caller-supplied
 * namespace pathname, forks, mounts, writes probes, or announces a Creator.
 * It enters exactly the manager-opened namespace and irrevocably removes setup
 * authority before the original helper image and argv can run in this PID.
 */
enum { NS_FD = 6, IMAGE_FD = 7, SETUP_FD = 8, MAX_FDS = 128 };
static const char setup_names[] =
    "hermit_group_id:hermit_group_format:hermit_group_enable:"
    "hermit_prepared_mount:hermit_prepared_image:hermit_prepared_setup";
static const char leaf_names[] =
    "hermit_group_id:hermit_group_format:hermit_group_enable";
static uint64_t stage, cutoff;
static int parent_pin = -1;

static _Noreturn void refuse(const char *message) {
    int saved = errno;
    (void)dprintf(STDERR_FILENO, "grouped namespace setup refused: %s (errno=%d)\n",
                  message, saved);
    _exit(125);
}
static void need(bool condition, const char *message) {
    if (!condition) refuse(message);
}
static uint64_t monotonic_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) < 0) refuse("monotonic clock");
    need(ts.tv_sec >= 0 && (uint64_t)ts.tv_sec <= UINT64_MAX / 1000000000,
         "monotonic overflow");
    return (uint64_t)ts.tv_sec * 1000000000 + (uint64_t)ts.tv_nsec;
}
static uint64_t decimal(const char *text) {
    uint64_t value = 0;
    need(text && *text && !(text[0] == '0' && text[1]), "canonical decimal");
    for (const unsigned char *p = (const unsigned char *)text; *p; ++p) {
        need(*p >= '0' && *p <= '9', "decimal digit");
        need(value <= (UINT64_MAX - (*p - '0')) / 10, "decimal overflow");
        value = value * 10 + (*p - '0');
    }
    return value;
}
static bool nonce(const char *s) {
    if (!s || strlen(s) != 32) return false;
    bool nonzero = false;
    for (unsigned i = 0; i < 32; ++i) {
        if (!((s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f')))
            return false;
        nonzero |= s[i] != '0';
    }
    return nonzero;
}
static struct stat fd_stat(int fd) {
    struct stat st;
    if (fstat(fd, &st) < 0) refuse("held descriptor stat");
    return st;
}
static bool same(const struct stat *a, const struct stat *b) {
    return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}
static int open_namespace(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) refuse("native namespace open");
    struct statfs fs;
    need(fstatfs(fd, &fs) == 0 && (unsigned long)fs.f_type == 0x6e736673UL,
         "actual nsfs descriptor");
    return fd;
}
static void close_fd(int fd) {
    if (close(fd) != 0) refuse("owned descriptor close");
}
static void check_deadline(void) {
    uint64_t now = monotonic_ns();
    need(now < stage && now < cutoff, "original wrapper/stage cutoff");
    if (parent_pin >= 0) {
        struct pollfd p = {.fd = parent_pin, .events = POLLIN};
        need(poll(&p, 1, 0) == 0 && p.revents == 0, "original controller terminal");
    }
}
static void check_initial_fds(void) {
    DIR *directory = opendir("/proc/self/fd");
    if (!directory) refuse("initial descriptor census");
    int own = dirfd(directory);
    need(own > SETUP_FD && own < MAX_FDS, "census descriptor bound");
    unsigned mask = 0;
    for (;;) {
        errno = 0;
        struct dirent *entry = readdir(directory);
        if (!entry) {
            need(errno == 0, "descriptor census read");
            break;
        }
        if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
        uint64_t fd = decimal(entry->d_name);
        if (fd == (uint64_t)own) continue;
        need(fd <= SETUP_FD, "unexpected inherited setup descriptor");
        mask |= 1U << fd;
    }
    need(closedir(directory) == 0 && mask == 511, "exact setup descriptor population");
    for (int fd = 3; fd <= SETUP_FD; ++fd)
        need(fcntl(fd, F_GETFD) == 0 && (fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDONLY,
             "manager descriptor flags");
}
static void close_authenticated_setup(void) {
    struct stat setup = fd_stat(SETUP_FD);
    need(S_ISREG(setup.st_mode) && (setup.st_mode & 0111) &&
             setup.st_size > 0 && setup.st_size <= 1048576,
         "held trusted setup image type and bound");
    int executable = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    need(executable > SETUP_FD && executable < MAX_FDS,
         "actual executing setup image description");
    struct stat running = fd_stat(executable);
    need(same(&setup, &running) &&
             (setup.st_mode & S_IFMT) == (running.st_mode & S_IFMT) &&
             setup.st_size == running.st_size,
         "held setup image matches actual executing image");
    close_fd(executable);
    // Close the extra manager role before acquiring the original peer pin.
    // The original helper still inherits exactly the three leaf descriptors.
    close_fd(SETUP_FD);
}
static void original_frame(const char *run) {
    struct ucred peer;
    socklen_t length = sizeof(peer);
    need(getsockopt(STDIN_FILENO, SOL_SOCKET, SO_PEERCRED, &peer, &length) == 0 &&
             length == sizeof(peer) && peer.pid > 1 && peer.pid != getpid() &&
             peer.uid == getuid() && peer.gid == getgid(),
         "original controller endpoint credentials");
    int kind = 0;
    length = sizeof(kind);
    need(getsockopt(STDIN_FILENO, SOL_SOCKET, SO_TYPE, &kind, &length) == 0 &&
             length == sizeof(kind) && kind == SOCK_SEQPACKET,
         "original seqpacket endpoint");
    int pass = 1;
    need(setsockopt(STDIN_FILENO, SOL_SOCKET, SO_PASSCRED, &pass, sizeof(pass)) == 0,
         "setup receive credentials");
    /* Pin the socket's original peer, not a process looked up by a reusable
     * numeric PID. A queued frame cannot substitute a later PID occupant. */
    length = sizeof(parent_pin);
    need(getsockopt(STDIN_FILENO, SOL_SOCKET, SO_PEERPIDFD, &parent_pin, &length) == 0 &&
             length == sizeof(parent_pin) && parent_pin >= 8 && parent_pin < MAX_FDS &&
             fcntl(parent_pin, F_GETFD) == FD_CLOEXEC,
         "original socket controller pin");
    char bytes[256];
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(struct ucred)) + CMSG_SPACE(sizeof(int))]; } control;
    struct iovec iov = {.iov_base = bytes, .iov_len = sizeof(bytes)};
    struct msghdr msg;
    ssize_t n;
    for (;;) {
        check_deadline();
        memset(&msg, 0, sizeof(msg));
        memset(&control, 0, sizeof(control));
        msg.msg_iov = &iov; msg.msg_iovlen = 1;
        msg.msg_control = &control; msg.msg_controllen = sizeof(control);
        n = recvmsg(STDIN_FILENO, &msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
        if (n >= 0) break;
        if (errno == EINTR) continue;
        need(errno == EAGAIN || errno == EWOULDBLOCK, "original cutoff frame receive");
        uint64_t now = monotonic_ns();
        need(now < cutoff, "setup frame cutoff");
        uint64_t left = cutoff - now;
        struct timespec wait = {.tv_sec = (time_t)(left / 1000000000), .tv_nsec = (long)(left % 1000000000)};
        struct pollfd p[2] = {{.fd = STDIN_FILENO, .events = POLLIN}, {.fd = parent_pin, .events = POLLIN}};
        int raw = ppoll(p, 2, &wait, NULL);
        if (raw < 0 && errno == EINTR) continue;
        need(raw > 0 && !p[1].revents && !(p[0].revents & ~(POLLIN | POLLHUP)),
             "original cutoff frame wait");
    }
    need(n > 0 && n < (ssize_t)sizeof(bytes) &&
             !(msg.msg_flags & ~MSG_CMSG_CLOEXEC) && !memchr(bytes, 0, (size_t)n),
         "complete original cutoff frame");
    struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
    need(cm && cm->cmsg_level == SOL_SOCKET && cm->cmsg_type == SCM_CREDENTIALS &&
             cm->cmsg_len == CMSG_LEN(sizeof(struct ucred)) && CMSG_NXTHDR(&msg, cm) == NULL,
         "exact zero-right setup credentials");
    struct ucred actual;
    memcpy(&actual, CMSG_DATA(cm), sizeof(actual));
    need(actual.pid == peer.pid && actual.uid == peer.uid && actual.gid == peer.gid,
         "cutoff frame original sender");
    bytes[n] = 0;
    char prefix[96];
    int prefix_length = snprintf(prefix, sizeof(prefix), "PREPARED_MOUNT_V1 nonce=%s cutoff=", run);
    need(prefix_length > 0 && prefix_length < (int)sizeof(prefix) && n > prefix_length + 1 &&
             !memcmp(bytes, prefix, (size_t)prefix_length) && bytes[n - 1] == '\n',
         "original cutoff frame grammar");
    bytes[n - 1] = 0;
    uint64_t parent_cutoff = decimal(bytes + prefix_length);
    need(parent_cutoff <= stage, "parent cutoff exceeds original stage");
    /* The local entry ceiling never moves; the actual parent's original cutoff
     * can only shorten it. This is not the helper's independent entry clock. */
    if (parent_cutoff < cutoff) cutoff = parent_cutoff;
    check_deadline();
}
static void verify_service(const char *unit) {
    char path[4096];
    int fd = open("/proc/self/cgroup", O_RDONLY | O_CLOEXEC);
    if (fd < 0) refuse("actual setup cgroup");
    ssize_t n = read(fd, path, sizeof(path));
    close_fd(fd);
    need(n > 4 && n < (ssize_t)sizeof(path) && !memcmp(path, "0::/", 4) && path[n - 1] == '\n',
         "actual cgroup framing");
    path[n - 1] = 0;
    char *name = strrchr(path, '/');
    need(name && !strcmp(name + 1, unit), "original leaf service membership");
    struct { int resource; rlim_t value; } limits[] = {
        {RLIMIT_NOFILE, 256}, {RLIMIT_FSIZE, 1048576}, {RLIMIT_CORE, 0}
    };
    for (unsigned i = 0; i < sizeof(limits) / sizeof(limits[0]); ++i) {
        struct rlimit value;
        need(getrlimit(limits[i].resource, &value) == 0 &&
                 value.rlim_cur == limits[i].value && value.rlim_max == limits[i].value,
             "original setup resource limit");
    }
}
int main(int argc, char **argv) {
    uint64_t entered = monotonic_ns();
    need(entered <= UINT64_MAX - 1000000000, "setup entry cutoff overflow");
    cutoff = entered + 1000000000;
    need(prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) == 0 && prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 0,
         "setup nondumpable protection");
    need(argc == 21 && !strcmp(argv[1], "--namespace") &&
             !strcmp(argv[4], "--user-namespace") && !strcmp(argv[7], "--root") &&
             !strcmp(argv[10], "--") && argv[11][0] == '/' &&
             !strcmp(argv[12], "--grouped-leaves-private-stdin-v1") &&
             !strcmp(argv[13], "--unit") && !strcmp(argv[15], "--run") &&
             !strcmp(argv[17], "--incarnation") && !strcmp(argv[19], "--deadline-ns"),
         "fixed prepared leaf argv");
    uint64_t ns_device = decimal(argv[2]), ns_inode = decimal(argv[3]);
    uint64_t user_device = decimal(argv[5]), user_inode = decimal(argv[6]);
    uint64_t root_device = decimal(argv[8]), root_inode = decimal(argv[9]);
    const char *unit = argv[14], *run = argv[16];
    need(strlen(unit) == 56 && !memcmp(unit, "hermit-accepted-", 16) &&
             !strcmp(unit + 48, ".service"), "exact prepared leaf unit");
    char unit_nonce[33]; memcpy(unit_nonce, unit + 16, 32); unit_nonce[32] = 0;
    need(nonce(unit_nonce) && nonce(run) && nonce(getenv("INVOCATION_ID")), "unit/run/invocation identity");
    uint64_t incarnation = decimal(argv[18]), decoded = 0;
    for (unsigned i = 0; i < 8; ++i) {
        char byte[3] = {run[i * 2], run[i * 2 + 1], 0};
        decoded |= (uint64_t)strtoul(byte, NULL, 16) << (i * 8);
    }
    need(incarnation != 0 && incarnation == decoded, "original incarnation");
    stage = decimal(argv[20]);
    need(stage > entered && stage - entered <= 20000000000ULL, "original outer20s");
    if (stage < cutoff) cutoff = stage;
    need(getuid() != 0 && getuid() == geteuid() && getgid() == getegid() &&
             prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1, "same-user NNP setup policy");
    struct hermit_grouped_namespace_caps initial_caps;
    uint64_t setup_caps = HERMIT_GROUPED_NAMESPACE_FINAL_CAPS | HERMIT_GROUPED_NAMESPACE_SETUP_CAPS;
    need(hermit_grouped_namespace_policy_read_caps(&initial_caps) == 0 &&
             initial_caps.inheritable == setup_caps && initial_caps.permitted == setup_caps &&
             initial_caps.effective == setup_caps && initial_caps.bounding == setup_caps &&
             initial_caps.ambient == setup_caps && initial_caps.no_new_privileges == 1,
         "exact setup capability sets before namespace entry");
    need(getenv("LISTEN_PID") && decimal(getenv("LISTEN_PID")) == (uint64_t)getpid() &&
             getenv("LISTEN_FDS") && !strcmp(getenv("LISTEN_FDS"), "6") &&
             getenv("LISTEN_FDNAMES") && !strcmp(getenv("LISTEN_FDNAMES"), setup_names),
         "exact manager-opened setup roles");
    check_initial_fds();
    close_authenticated_setup();
    verify_service(unit);
    original_frame(run);
    struct statfs nsfs;
    struct stat ns = fd_stat(NS_FD), image = fd_stat(IMAGE_FD);
    need(fstatfs(NS_FD, &nsfs) == 0 && (unsigned long)nsfs.f_type == 0x6e736673UL &&
             ioctl(NS_FD, NS_GET_NSTYPE) == CLONE_NEWNS &&
             (uint64_t)ns.st_dev == ns_device && (uint64_t)ns.st_ino == ns_inode,
         "held original source mount namespace");
    int current = open_namespace("/proc/self/ns/mnt");
    struct stat host = fd_stat(current);
    need(!same(&host, &ns), "prepared namespace must differ from setup host namespace");
    close_fd(current);
    int user = ioctl(NS_FD, NS_GET_USERNS);
    need(user >= 8 && user < MAX_FDS, "prepared namespace owning userns");
    struct stat user_st = fd_stat(user);
    current = open_namespace("/proc/self/ns/user");
    struct stat initial_user = fd_stat(current);
    need(ioctl(current, NS_GET_NSTYPE) == CLONE_NEWUSER && same(&user_st, &initial_user) &&
             (uint64_t)user_st.st_dev == user_device && (uint64_t)user_st.st_ino == user_inode,
         "actual original initial user namespace");
    close_fd(current); close_fd(user);
    unsigned char elf[20];
    need(S_ISREG(image.st_mode) && (image.st_mode & 0111) && image.st_size >= 64 &&
             image.st_size <= 512 * 1024 * 1024 && pread(IMAGE_FD, elf, sizeof(elf), 0) == sizeof(elf) &&
             !memcmp(elf, "\177ELF\2\1", 6) && elf[18] == 62 && elf[19] == 0,
         "held original helper image");
    need(fcntl(IMAGE_FD, F_SETFD, FD_CLOEXEC) == 0, "helper image exec-close");
    check_deadline();
    need(setns(NS_FD, CLONE_NEWNS) == 0, "enter prepared source namespace");
    uint64_t joined = monotonic_ns();
    current = open_namespace("/proc/self/ns/mnt");
    struct stat actual = fd_stat(current), root;
    need(same(&actual, &ns) && stat("/", &root) == 0 &&
             (uint64_t)root.st_dev == root_device && (uint64_t)root.st_ino == root_inode,
         "actual prepared namespace and root");
    close_fd(current); close_fd(NS_FD);
    check_deadline();
    need(hermit_grouped_namespace_policy_drop_setup_caps() == 0, "irreversible exact capability drop");
    need(hermit_grouped_namespace_policy_install_filter() == 0, "irreversible original namespace filter");
    check_deadline();
    close_fd(parent_pin); parent_pin = -1;
    need(setenv("LISTEN_FDS", "3", 1) == 0 && setenv("LISTEN_FDNAMES", leaf_names, 1) == 0,
         "restore original three-role environment");
    (void)dprintf(STDERR_FILENO,
        "grouped namespace setup complete: entered=%" PRIu64 " joined=%" PRIu64
        " prepared=%" PRIu64 " cutoff=%" PRIu64 "\n",
        entered, joined, monotonic_ns(), cutoff);
    check_deadline();
    extern char **environ;
    fexecve(IMAGE_FD, &argv[11], environ);
    refuse("original helper exec");
}
