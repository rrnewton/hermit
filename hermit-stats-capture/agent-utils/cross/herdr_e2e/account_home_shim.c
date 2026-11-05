/*
 * Preloaded into both herdr-run editions by the end-to-end differential (cross/herdr_differential.py).
 *
 * herdr-run finds Herdr only in fixed places under the account home that the account database
 * reports, never through PATH or HOME, so there is no option or variable that points it at a fake.
 * This shim changes the one thing that decides it, without changing herdr-run: getpwuid_r and
 * getpwuid report the directory in HERDR_RUN_E2E_ACCOUNT_HOME as the current account's home. The
 * harness installs a fake Herdr under it, and both editions then run their production client
 * against that fake.
 *
 * It is also a guard. execve and its relatives refuse, with EPERM, to start any program named
 * systemd-run, and any program named herdr outside that account home, and append the refused path
 * to HERDR_RUN_E2E_REFUSALS. So a case can never reach the real systemd-run or a real Herdr, even
 * if it makes an edition try; the harness fails any case whose refusal log is not empty.
 *
 * Built by the harness with the system C compiler at test time. Nothing else uses it.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pwd.h>
#include <spawn.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

extern char **environ;

static char account_home[4096];
static size_t account_home_length;
static char refusal_log[4096];

typedef int (*getpwuid_r_function)(uid_t, struct passwd *, char *, size_t, struct passwd **);
typedef struct passwd *(*getpwuid_function)(uid_t);
typedef int (*execve_function)(const char *, char *const[], char *const[]);
typedef int (*execvp_function)(const char *, char *const[]);
typedef int (*execvpe_function)(const char *, char *const[], char *const[]);
typedef int (*posix_spawn_function)(pid_t *, const char *, const posix_spawn_file_actions_t *,
                                    const posix_spawnattr_t *, char *const[], char *const[]);

static getpwuid_r_function real_getpwuid_r;
static getpwuid_function real_getpwuid;
static execve_function real_execve;
static execvp_function real_execvp;
static execvpe_function real_execvpe;
static posix_spawn_function real_posix_spawn;
static posix_spawn_function real_posix_spawnp;

static void copy_variable(const char *name, char *target, size_t size) {
    const char *value = getenv(name);
    if (value != NULL && strlen(value) < size) {
        strcpy(target, value);
    }
}

/* Everything the interposed calls need is resolved here, at load time, so the exec wrappers
 * make only async-signal-safe calls when they run between fork and exec. */
__attribute__((constructor)) static void initialise(void) {
    copy_variable("HERDR_RUN_E2E_ACCOUNT_HOME", account_home, sizeof account_home);
    account_home_length = strlen(account_home);
    copy_variable("HERDR_RUN_E2E_REFUSALS", refusal_log, sizeof refusal_log);
    real_getpwuid_r = (getpwuid_r_function)dlsym(RTLD_NEXT, "getpwuid_r");
    real_getpwuid = (getpwuid_function)dlsym(RTLD_NEXT, "getpwuid");
    real_execve = (execve_function)dlsym(RTLD_NEXT, "execve");
    real_execvp = (execvp_function)dlsym(RTLD_NEXT, "execvp");
    real_execvpe = (execvpe_function)dlsym(RTLD_NEXT, "execvpe");
    real_posix_spawn = (posix_spawn_function)dlsym(RTLD_NEXT, "posix_spawn");
    real_posix_spawnp = (posix_spawn_function)dlsym(RTLD_NEXT, "posix_spawnp");
}

int getpwuid_r(uid_t uid, struct passwd *record, char *buffer, size_t size,
               struct passwd **result) {
    if (real_getpwuid_r == NULL) {
        real_getpwuid_r = (getpwuid_r_function)dlsym(RTLD_NEXT, "getpwuid_r");
    }
    int status = real_getpwuid_r(uid, record, buffer, size, result);
    if (status == 0 && *result != NULL && account_home_length > 0 && uid == getuid()) {
        record->pw_dir = account_home;
    }
    return status;
}

struct passwd *getpwuid(uid_t uid) {
    if (real_getpwuid == NULL) {
        real_getpwuid = (getpwuid_function)dlsym(RTLD_NEXT, "getpwuid");
    }
    struct passwd *record = real_getpwuid(uid);
    if (record != NULL && account_home_length > 0 && uid == getuid()) {
        record->pw_dir = account_home;
    }
    return record;
}

static int refused(const char *path) {
    if (path == NULL) {
        return 0;
    }
    const char *slash = strrchr(path, '/');
    const char *base = slash == NULL ? path : slash + 1;
    if (strcmp(base, "systemd-run") == 0) {
        return 1;
    }
    if (strcmp(base, "herdr") == 0) {
        return account_home_length == 0 || strncmp(path, account_home, account_home_length) != 0 ||
               path[account_home_length] != '/';
    }
    return 0;
}

static void record_refusal(const char *path) {
    if (refusal_log[0] == '\0') {
        return;
    }
    int descriptor = open(refusal_log, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
    if (descriptor < 0) {
        return;
    }
    ssize_t ignored = write(descriptor, path, strlen(path));
    ignored = write(descriptor, "\n", 1);
    (void)ignored;
    close(descriptor);
}

int execve(const char *path, char *const argv[], char *const envp[]) {
    if (refused(path)) {
        record_refusal(path);
        errno = EPERM;
        return -1;
    }
    if (real_execve == NULL) {
        /* Only when called before the constructor ran; never between fork and exec. */
        real_execve = (execve_function)dlsym(RTLD_NEXT, "execve");
    }
    return real_execve(path, argv, envp);
}

int execv(const char *path, char *const argv[]) { return execve(path, argv, environ); }

int execvp(const char *file, char *const argv[]) {
    if (refused(file)) {
        record_refusal(file);
        errno = EPERM;
        return -1;
    }
    return real_execvp(file, argv);
}

int execvpe(const char *file, char *const argv[], char *const envp[]) {
    if (refused(file)) {
        record_refusal(file);
        errno = EPERM;
        return -1;
    }
    return real_execvpe(file, argv, envp);
}

int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *actions,
                const posix_spawnattr_t *attributes, char *const argv[], char *const envp[]) {
    if (refused(path)) {
        record_refusal(path);
        return EPERM;
    }
    return real_posix_spawn(pid, path, actions, attributes, argv, envp);
}

int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                 const posix_spawnattr_t *attributes, char *const argv[], char *const envp[]) {
    if (refused(file)) {
        record_refusal(file);
        return EPERM;
    }
    return real_posix_spawnp(pid, file, actions, attributes, argv, envp);
}
