/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Looks up the CPUs and NUMA nodes other than number 0 that the sysfs
// listings leave out, and registers an rseq area, and prints what each call
// returns. Under Hermit, which presents one CPU and one node, every lookup
// fails with ENOENT and rseq with ENOSYS; natively, on a host with more than
// one CPU, the cpu1 lookups succeed.
//
// The lookups name the absent entry absolutely, below it, relative to a
// directory descriptor, through /sys/bus (including the processor driver's
// link), with stat, access and readlink, and the node0 and cpu0 names stay
// found. rseq is registered with an aligned
// 32-byte area and the signature glibc uses, so the kernel would accept it.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void report(const char *what, int rc) {
    if (rc < 0) {
        printf("%s: %s\n", what, strerrorname_np(errno));
    } else {
        printf("%s: ok\n", what);
    }
}

static void open_and_close(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    report(path, fd);
    if (fd >= 0) {
        close(fd);
    }
}

struct rseq_area {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    char end[];
} __attribute__((aligned(32)));

int main(void) {
    open_and_close("/sys/devices/system/node/node1/cpulist");
    open_and_close("/sys/devices/system/cpu/cpu1/topology/core_id");
    open_and_close("/sys/devices/system/cpu/cpu0/topology/core_id");
    open_and_close("/sys/devices/system/node/node0/cpulist");

    struct stat st;
    report("stat /sys/devices/system/cpu/cpu1",
           stat("/sys/devices/system/cpu/cpu1", &st));
    report("access /sys/devices/system/node/node1",
           access("/sys/devices/system/node/node1", F_OK));
    char target[256];
    report("readlink /sys/bus/cpu/devices/cpu1",
           (int)readlink("/sys/bus/cpu/devices/cpu1", target, sizeof(target)));
    report("readlink /sys/bus/cpu/devices/cpu0",
           (int)readlink("/sys/bus/cpu/devices/cpu0", target, sizeof(target)));
    report("readlink /sys/bus/cpu/drivers/processor/cpu1",
           (int)readlink("/sys/bus/cpu/drivers/processor/cpu1", target, sizeof(target)));

    int directory = open("/sys/devices/system/cpu", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory < 0) {
        perror("open /sys/devices/system/cpu");
        return 1;
    }
    int online = openat(directory, "cpu1/online", O_RDONLY | O_CLOEXEC);
    report("openat cpu1/online", online);
    if (online >= 0) {
        close(online);
    }
    report("fstatat cpu1", fstatat(directory, "cpu1", &st, 0));
    close(directory);

    static struct rseq_area area;
    memset(&area, 0, sizeof(area));
    area.cpu_id = (uint32_t)-1;
    report("rseq", (int)syscall(SYS_rseq, &area, sizeof(area), 0, 0x53053053));
    printf("rseq cpu_id: %d\n", (int)area.cpu_id);
    return 0;
}
