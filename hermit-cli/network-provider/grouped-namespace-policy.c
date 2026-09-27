/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "grouped-namespace-policy.h"

#include <errno.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

#if !defined(__linux__) || !defined(__x86_64__) || defined(__ILP32__)
#error "The grouped namespace setup policy supports the x86-64 Linux host only"
#endif

/* systemd-260.4 RestrictNamespaces=yes: clone3 => ENOSYS, setns => EPERM,
 * clone/unshare with any namespace bit => EPERM. Its separate i386/x32/x86-64
 * contexts use ACT_BADARCH=ALLOW. Preserve that composition, including x32's
 * shared audit architecture and distinct syscall-number bit. Unknown audit
 * architectures therefore ALLOW here; they are not a supported host claim.
 * The low-word mask matches systemd's 64-bit MASKED_EQ with zero high mask. */
static const struct sock_filter namespace_filter[] = {
    /* 0 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
    /* 1 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_I386, 9, 0),
    /* 2 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
    /* 3 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    /* 4 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    /* 5 */ BPF_STMT(BPF_ALU | BPF_AND | BPF_K, ~UINT32_C(0x40000000)),
    /* 6 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 435, 13, 0),
    /* 7 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 308, 11, 0),
    /* 8 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 56, 8, 0),
    /* 9 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 272, 7, 0),
    /* 10 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    /* 11 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    /* 12 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 435, 7, 0),
    /* 13 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 346, 5, 0),
    /* 14 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 120, 2, 0),
    /* 15 */ BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 310, 1, 0),
    /* 16 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    /* 17 */ BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
    /* 18 */ BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, UINT32_C(0x7e020080), 0, 2),
    /* 19 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
    /* 20 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
    /* 21 */ BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
};

const struct sock_filter *hermit_grouped_namespace_policy_filter(size_t *count) {
    if (count != NULL) {
        *count = sizeof(namespace_filter) / sizeof(namespace_filter[0]);
    }
    return namespace_filter;
}

int hermit_grouped_namespace_policy_install_filter(void) {
    int nnp = prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    if (nnp < 0) {
        return -1;
    }
    if (nnp != 1) {
        errno = EPERM;
        return -1;
    }
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(namespace_filter) / sizeof(namespace_filter[0])),
        .filter = (struct sock_filter *)namespace_filter,
    };
    return (int)syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program);
}

static int read_cap_bit(int operation, unsigned cap) {
    int result = prctl(operation, cap, 0, 0, 0);
    if (result < 0 && errno == EINVAL && cap > CAP_LAST_CAP) {
        return 0;
    }
    return result;
}

int hermit_grouped_namespace_policy_read_caps(struct hermit_grouped_namespace_caps *out) {
    if (out == NULL) {
        errno = EINVAL;
        return -1;
    }
    memset(out, 0, sizeof(*out));
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2] = {{0}};
    if (syscall(SYS_capget, &header, data) != 0) {
        return -1;
    }
    out->inheritable = data[0].inheritable | ((uint64_t)data[1].inheritable << 32);
    out->permitted = data[0].permitted | ((uint64_t)data[1].permitted << 32);
    out->effective = data[0].effective | ((uint64_t)data[1].effective << 32);
    for (unsigned cap = 0; cap < 64; cap++) {
        int bounding = read_cap_bit(PR_CAPBSET_READ, cap);
        if (bounding < 0) {
            return -1;
        }
        int ambient = prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, cap, 0, 0);
        if (ambient < 0 && errno == EINVAL && cap > CAP_LAST_CAP) {
            ambient = 0;
        }
        if (ambient < 0) {
            return -1;
        }
        if (bounding != 0) {
            out->bounding |= UINT64_C(1) << cap;
        }
        if (ambient != 0) {
            out->ambient |= UINT64_C(1) << cap;
        }
    }
    out->no_new_privileges = prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    return out->no_new_privileges < 0 ? -1 : 0;
}

static int exact_caps(const struct hermit_grouped_namespace_caps *caps, uint64_t expected) {
    return caps->no_new_privileges == 1 && caps->inheritable == expected &&
        caps->permitted == expected && caps->effective == expected &&
        caps->bounding == expected && caps->ambient == expected;
}

int hermit_grouped_namespace_policy_drop_setup_caps(void) {
    struct hermit_grouped_namespace_caps before, after;
    if (hermit_grouped_namespace_policy_read_caps(&before) < 0) {
        return -1;
    }
    if (geteuid() == 0 || !exact_caps(&before,
            HERMIT_GROUPED_NAMESPACE_FINAL_CAPS | HERMIT_GROUPED_NAMESPACE_SETUP_CAPS)) {
        errno = EPERM;
        return -1;
    }
    static const unsigned setup[] = {CAP_SYS_ADMIN, CAP_SYS_CHROOT, CAP_SETPCAP};
    for (size_t i = 0; i < sizeof(setup) / sizeof(setup[0]); i++) {
        if (prctl(PR_CAPBSET_DROP, setup[i], 0, 0, 0) != 0) {
            return -1;
        }
    }
    for (size_t i = 0; i < sizeof(setup) / sizeof(setup[0]); i++) {
        if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, setup[i], 0, 0) != 0) {
            return -1;
        }
    }
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    for (unsigned i = 0; i < 2; i++) {
        uint32_t word = (uint32_t)(HERMIT_GROUPED_NAMESPACE_FINAL_CAPS >> (i * 32));
        data[i] = (struct __user_cap_data_struct){word, word, word};
    }
    if (syscall(SYS_capset, &header, data) != 0 ||
            hermit_grouped_namespace_policy_read_caps(&after) != 0) {
        return -1;
    }
    if (!exact_caps(&after, HERMIT_GROUPED_NAMESPACE_FINAL_CAPS)) {
        errno = EPERM;
        return -1;
    }
    return 0;
}
