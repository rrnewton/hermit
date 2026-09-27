/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-namespace-policy.h"

#include <assert.h>
#include <errno.h>
#include <linux/audit.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* Evaluate the actual production instructions, not a duplicate predicate. */
static uint32_t evaluate(const struct seccomp_data *input) {
    size_t count;
    const struct sock_filter *program = hermit_grouped_namespace_policy_filter(&count);
    uint32_t accumulator = 0;
    for (size_t pc = 0; pc < count;) {
        const struct sock_filter *insn = &program[pc++];
        switch (insn->code) {
        case BPF_LD | BPF_W | BPF_ABS:
            assert(insn->k <= sizeof(*input) - sizeof(accumulator));
            memcpy(&accumulator, (const char *)input + insn->k, sizeof(accumulator));
            break;
        case BPF_ALU | BPF_AND | BPF_K:
            accumulator &= insn->k;
            break;
        case BPF_JMP | BPF_JEQ | BPF_K:
            pc += accumulator == insn->k ? insn->jt : insn->jf;
            break;
        case BPF_JMP | BPF_JSET | BPF_K:
            pc += (accumulator & insn->k) != 0 ? insn->jt : insn->jf;
            break;
        case BPF_RET | BPF_K:
            return insn->k;
        default:
            assert(!"unexpected production BPF instruction");
        }
        assert(pc < count);
    }
    assert(!"filter did not return");
    return 0;
}

static unsigned vectors;
static void expected(uint32_t arch, uint32_t number, uint64_t flags, uint32_t result) {
    struct seccomp_data input = {.nr = (int)number, .arch = arch, .args = {flags}};
    assert(evaluate(&input) == result);
    vectors++;
}

static void abi_rules(void) {
    /* Independent installed ABI syscall literals, not production table values. */
    static const struct {
        uint32_t arch, bit, clone, unshare, setns, clone3, getpid;
    } abi[] = {
        {UINT32_C(0x40000003), 0, 120, 310, 346, 435, 20},
        {UINT32_C(0xc000003e), UINT32_C(0x40000000), 56, 272, 308, 435, 39},
        {UINT32_C(0xc000003e), 0, 56, 272, 308, 435, 39},
    };
    static const uint64_t ns[] = {
        UINT64_C(0x02000000), UINT64_C(0x08000000), UINT64_C(0x40000000),
        UINT64_C(0x00020000), UINT64_C(0x20000000), UINT64_C(0x10000000),
        UINT64_C(0x04000000), UINT64_C(0x00000080),
    };
    for (size_t i = 0; i < sizeof(abi) / sizeof(abi[0]); i++) {
        uint32_t calls[] = {abi[i].clone, abi[i].unshare};
        for (size_t j = 0; j < 2; j++) {
            uint32_t number = abi[i].bit | calls[j];
            for (size_t k = 0; k < sizeof(ns) / sizeof(ns[0]); k++) {
                expected(abi[i].arch, number, ns[k], SECCOMP_RET_ERRNO | EPERM);
            }
            expected(abi[i].arch, number, 0, SECCOMP_RET_ALLOW);
            expected(abi[i].arch, number, UINT64_C(0xffffffff00000000), SECCOMP_RET_ALLOW);
            expected(abi[i].arch, number, 17, SECCOMP_RET_ALLOW);
            expected(abi[i].arch, number, UINT64_MAX, SECCOMP_RET_ERRNO | EPERM);
        }
        expected(abi[i].arch, abi[i].bit | abi[i].setns, 0, SECCOMP_RET_ERRNO | EPERM);
        expected(abi[i].arch, abi[i].bit | abi[i].setns, UINT64_MAX, SECCOMP_RET_ERRNO | EPERM);
        expected(abi[i].arch, abi[i].bit | abi[i].clone3, 0, SECCOMP_RET_ERRNO | ENOSYS);
        expected(abi[i].arch, abi[i].bit | abi[i].clone3, UINT64_MAX, SECCOMP_RET_ERRNO | ENOSYS);
        expected(abi[i].arch, abi[i].bit | abi[i].getpid, UINT64_MAX, SECCOMP_RET_ALLOW);
    }
    expected(UINT32_C(0xc00000b7), 435, UINT64_MAX, SECCOMP_RET_ALLOW);
    expected(UINT32_C(0xc000003e), UINT32_C(0x80000038), UINT64_MAX, SECCOMP_RET_ALLOW);
    assert(vectors == 89);
}

static void missing_nnp(void) {
    assert(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 0);
    int original = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    assert(original >= 0);
    errno = 0;
    assert(hermit_grouped_namespace_policy_install_filter() == -1 && errno == EPERM);
    assert(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 0);
    assert(prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == original);
}

static void refused_capability_drop(void) {
    struct hermit_grouped_namespace_caps before, after;
    assert(geteuid() != 0);
    assert(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    assert(hermit_grouped_namespace_policy_read_caps(&before) == 0);
    assert(before.effective == 0 && before.permitted == 0 && before.ambient == 0);
    errno = 0;
    assert(hermit_grouped_namespace_policy_drop_setup_caps() == -1 && errno == EPERM);
    assert(hermit_grouped_namespace_policy_read_caps(&after) == 0);
    assert(memcmp(&before, &after, sizeof(before)) == 0);
}

static void native_filter(void) {
    struct hermit_grouped_namespace_caps before, after;
    assert(geteuid() != 0);
    errno = 0;
    assert(syscall(SYS_setns, -1, 0) == -1 && errno == EBADF);
    errno = 0;
    assert(syscall(SYS_clone3, NULL, 88) == -1 && errno == EFAULT);
    errno = 0;
    long unshare_before = syscall(SYS_unshare, 0);
    int unshare_errno = errno;
    assert(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    assert(hermit_grouped_namespace_policy_read_caps(&before) == 0);
    assert(hermit_grouped_namespace_policy_install_filter() == 0);
    assert(prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == SECCOMP_MODE_FILTER);
    assert(hermit_grouped_namespace_policy_read_caps(&after) == 0);
    assert(memcmp(&before, &after, sizeof(before)) == 0);
    errno = 0;
    assert(syscall(SYS_setns, -1, 0) == -1 && errno == EPERM);
    errno = 0;
    assert(syscall(SYS_setns, -1, 0x00020000) == -1 && errno == EPERM);
    errno = 0;
    assert(syscall(SYS_clone3, NULL, 88) == -1 && errno == ENOSYS);
    /* x32 reaches the BPF predicate even when the kernel disables that ABI. */
    errno = 0;
    assert(syscall(UINT32_C(0x40000000) | 308, -1, 0) == -1 && errno == EPERM);
    errno = 0;
    assert(syscall(SYS_unshare, 0) == unshare_before);
    assert(errno == unshare_errno);
    assert(syscall(SYS_getpid) == getpid());
}

static void child_case(const char *name, void (*control)(void)) {
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        alarm(2);
        control();
        _exit(0);
    }
    int status;
    assert(waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    printf("%s: natural0\n", name);
}

int main(void) {
    abi_rules();
    printf("production-filter ABI vectors: %u\n", vectors);
    child_case("missing-NNP refuses without filter installation", missing_nnp);
    child_case("missing setup capabilities refuse without mutation", refused_capability_drop);
    child_case("native setns/clone3/x32 refusal and ordinary syscall allowance", native_filter);
    errno = 0;
    assert(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD);
    return 0;
}
