/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_NAMESPACE_POLICY_H
#define HERMIT_GROUPED_NAMESPACE_POLICY_H

#include <linux/filter.h>
#include <stddef.h>
#include <stdint.h>

#define HERMIT_GROUPED_NAMESPACE_FINAL_CAPS \
    ((UINT64_C(1) << 12) | (UINT64_C(1) << 19) | \
     (UINT64_C(1) << 24) | (UINT64_C(1) << 38) | (UINT64_C(1) << 39))
#define HERMIT_GROUPED_NAMESPACE_SETUP_CAPS \
    ((UINT64_C(1) << 8) | (UINT64_C(1) << 18) | (UINT64_C(1) << 21))

struct hermit_grouped_namespace_caps {
    uint64_t inheritable, permitted, effective, bounding, ambient;
    int no_new_privileges;
};

/* Descriptive readback only. On failure the output may be partial and grants
 * no authority. These routines do not authenticate a namespace, executable,
 * identity or deadline, and do not enter a namespace or execute another image. */
int hermit_grouped_namespace_policy_read_caps(struct hermit_grouped_namespace_caps *);

/* Same-user trusted setup only, after authenticated setns and setup-FD closure.
 * Require actual NNP=1 and exact original five + three setup capabilities in
 * all five sets, then remove all three setup capabilities from every set.
 * A failure may follow irreversible partial drops: the caller must retain the
 * first error, record readback and terminate; it must never retry or exec. */
int hermit_grouped_namespace_policy_drop_setup_caps(void);

/* Stack the installed systemd RestrictNamespaces=yes semantics for all three
 * ABIs on the supported x86-64 host. Requires actual NNP=1. No existing filter
 * is removed. Any error forbids the caller from executing the final helper. */
int hermit_grouped_namespace_policy_install_filter(void);

/* Exact immutable instructions used by install_filter, exposed for inspection
 * and interpreter tests. Obtaining them is not evidence of installation. */
const struct sock_filter *hermit_grouped_namespace_policy_filter(size_t *count);

#endif
