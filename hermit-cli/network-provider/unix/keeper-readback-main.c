/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include "keeper-channel.h"
#include "keeper-readback.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static int wait_request(int channel,int parent,uint64_t deadline) {
    for(;;) {
        struct timespec now;if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
        if(now.tv_sec<0 || now.tv_nsec<0 || now.tv_nsec>=1000000000 ||
           (uint64_t)now.tv_sec>(UINT64_MAX-(uint64_t)now.tv_nsec)/1000000000ULL) {errno=EOVERFLOW;return -1;}
        uint64_t ns=(uint64_t)now.tv_sec*1000000000ULL+(uint64_t)now.tv_nsec;
        if(ns>=deadline) {errno=ETIMEDOUT;return -1;}
        uint64_t remaining=deadline-ns;
        int timeout=remaining>=50000000ULL?50:(int)(remaining/1000000ULL);
        struct pollfd p[2]={{channel,POLLIN,0},{parent,POLLIN,0}};
        int ready=poll(p,2,timeout);
        if(ready<0 && errno==EINTR)continue;
        if(ready<0)return -1;
        if(p[1].revents || p[0].revents&(POLLERR|POLLNVAL|POLLHUP)) {errno=EPIPE;return -1;}
        if(p[0].revents&POLLIN) {
            if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
            if(now.tv_sec<0 || now.tv_nsec<0 || now.tv_nsec>=1000000000 ||
               (uint64_t)now.tv_sec>(UINT64_MAX-(uint64_t)now.tv_nsec)/1000000000ULL) {errno=EOVERFLOW;return -1;}
            ns=(uint64_t)now.tv_sec*1000000000ULL+(uint64_t)now.tv_nsec;
            if(ns>=deadline) {errno=ETIMEDOUT;return -1;}
            return 0;
        }
    }
}
/* Explicit later recovery observation, never an original-close certificate.
 * At most one original inventory (72 IDs), no filenames, no BPF mutation. */
static int recovery_actor(void) {
    /* Only this new metadata-only mode emits an actor record. Its private
     * systemd-run --pipe stdout binds the record to the actual launched helper;
     * a launch intent or a later unit-name lookup cannot create this evidence. */
    const char *invocation = getenv("INVOCATION_ID");
    if (!invocation || strlen(invocation) != 32 ||
        strspn(invocation, "0123456789abcdef") != 32 ||
        strspn(invocation, "0") == 32) return -1;
    char text[4096], path[4096];
    int fd = open("/proc/self/cgroup", O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    ssize_t n = read(fd, text, sizeof(text) - 1);
    int saved = errno;
    if (close(fd)) return -1;
    errno = saved;
    if (n <= 0 || n == (ssize_t)sizeof(text) - 1) return -1;
    text[n] = 0;
    /* Supported deployment is unified cgroup v2 with one exact record. */
    if (strncmp(text, "0::/", 4) || text[n - 1] != '\n' ||
        memchr(text, '\n', (size_t)n - 1) || memchr(text, 0, (size_t)n)) return -1;
    text[n - 1] = 0;
    const char *group = text + 3;
    if (strpbrk(group, " \t\r\\") || strstr(group, "/../") ||
        strstr(group, "/./") || strstr(group, "//")) return -1;
    int size = snprintf(path, sizeof(path), "/sys/fs/cgroup%s", group);
    if (size < 0 || size >= (int)sizeof(path)) return -1;
    fd = open(path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    struct stat st;
    int result = fstat(fd, &st);
    saved = errno;
    if (close(fd)) return -1;
    errno = saved;
    if (result || !S_ISDIR(st.st_mode) || !st.st_ino) return -1;
    if (printf("recovery-actor-v1 %s %llu %llu %s\n", invocation,
               (unsigned long long)st.st_dev, (unsigned long long)st.st_ino, path) < 0 ||
        fflush(stdout)) return -1;
    return 0;
}
static int recovery(int argc, char **argv) {
    if (argc < 4 || argc > 3 + UG_INVENTORY_MAX) return 125;
    for (const char *p = argv[2]; *p; ++p)
        if (*p < '0' || *p > '9') return 125;
    errno = 0; char *end = NULL;
    uint64_t deadline = strtoull(argv[2], &end, 10);
    if (errno || !argv[2][0] || !deadline || !end || *end) return 125;
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) || now.tv_sec < 0 ||
        now.tv_nsec < 0 || now.tv_nsec >= 1000000000 ||
        (uint64_t)now.tv_sec > (UINT64_MAX - (uint64_t)now.tv_nsec) / 1000000000ULL) return 125;
    uint64_t begin = (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
    if (deadline <= begin || deadline - begin > 15000000000ULL) return 125;
    struct ug_plain_id ids[UG_INVENTORY_MAX] = {{0}};
    for (int i = 3; i < argc; ++i) {
        const char *text = argv[i];
        if (text[0] < '0' || text[0] > '2' || text[1] != ':' || !text[2]) return 125;
        for (const char *p = text + 2; *p; ++p)
            if (*p < '0' || *p > '9') return 125;
        errno = 0; unsigned long id = strtoul(text + 2, &end, 10);
        if (errno || !id || id > UINT32_MAX || !end || *end) return 125;
        ids[i - 3] = (struct ug_plain_id){(uint32_t)(text[0] - '0'), (uint32_t)id};
    }
    /* Same private channel/held-parent/deadline lifetime as original readback.
     * The first packet carries the exact owner pidfd, not release authority.
     * Its actor then stays live until the owner captures the original cgroup. */
    int channel = fcntl(0, F_DUPFD_CLOEXEC, 3);
    if (channel < 0 || close(0)) return 125;
    int type = 0; socklen_t size = sizeof(type);
    if (getsockopt(channel, SOL_SOCKET, SO_TYPE, &type, &size) ||
        type != SOCK_SEQPACKET || wait_request(channel, -1, deadline)) return 125;
    unsigned char bootstrap = 1;
    char control[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec parent_io = {&bootstrap, sizeof(bootstrap)};
    struct msghdr parent_message = {.msg_iov = &parent_io, .msg_iovlen = 1,
        .msg_control = control, .msg_controllen = sizeof(control)};
    ssize_t initialized = recvmsg(channel, &parent_message, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
    struct cmsghdr *parent_right = CMSG_FIRSTHDR(&parent_message);
    if (initialized != 1 || bootstrap != 0 ||
        (parent_message.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) || !parent_right ||
        parent_right->cmsg_level != SOL_SOCKET || parent_right->cmsg_type != SCM_RIGHTS ||
        parent_right->cmsg_len != CMSG_LEN(sizeof(int)) ||
        CMSG_NXTHDR(&parent_message, parent_right)) return 125;
    int parent = -1; memcpy(&parent, CMSG_DATA(parent_right), sizeof(parent));
    if (parent < 0 || recovery_actor() || wait_request(channel, parent, deadline)) return 125;
    unsigned char release = 0;
    struct iovec io = {&release, sizeof(release)};
    struct msghdr message = {.msg_iov = &io, .msg_iovlen = 1};
    ssize_t received = recvmsg(channel, &message, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
    if (received != 1 || release != 1 ||
        (message.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) || close(channel) || close(parent)) return 125;
    uint64_t observed = 0, passes = 0;
    if (ug_query_absence(ids, (uint32_t)(argc - 3), begin, deadline, &observed, &passes)) return 125;
    if (printf("recovery-v1 %u %llu %llu %llu %llu\n", (unsigned)(argc - 3),
            (unsigned long long)begin, (unsigned long long)deadline,
            (unsigned long long)observed, (unsigned long long)passes) < 0) return 125;
    return 0;
}
/* This executable links only libc + the channel codec + metadata reader. No
 * libbpf or session loader is linked, so the privileged entry cannot load policy. */
int main(int argc,char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "--recover-ids-before-ns")) return recovery(argc, argv);
    if(argc!=3 || strcmp(argv[1],"--readback-before-ns") || !argv[2][0])return 125;
    for(const char *p=argv[2];*p;p++)if(*p<'0' || *p>'9')return 125;
    errno=0;char *end=NULL;uint64_t deadline=strtoull(argv[2],&end,10);
    if(errno || !deadline || !end || *end)return 125;
    int channel=fcntl(0,F_DUPFD_CLOEXEC,3);if(channel<0 || close(0))return 125;
    int type=0;socklen_t size=sizeof(type);
    if(getsockopt(channel,SOL_SOCKET,SO_TYPE,&type,&size) || type!=SOCK_SEQPACKET)return 125;
    int self=(int)syscall(SYS_pidfd_open,syscall(SYS_gettid),UG_PIDFD_THREAD);if(self<0)return 125;
    struct ug_packet request,response;struct ug_inventory inventory;
    if(wait_request(channel,-1,deadline) || ug_channel_receive(channel,&request))return 125;
    if(request.frame.operation!=UG_READBACK_INIT || request.frame.sequence!=1 || request.count!=2 ||
       request.frame.values[1]!=deadline || ug_read_inventory(request.fds[0],&inventory) ||
       request.frame.incarnation!=inventory.incarnation || request.frame.values[0]!=inventory.proof_sequence)return 125;
    int inventory_fd=request.fds[0],parent=request.fds[1];
    memset(&response,0,sizeof(response));response.frame=request.frame;
    response.frame.operation|=UG_RESPONSE;response.count=response.frame.rights=1;response.fds[0]=self;
    if(ug_channel_send(channel,&response))return 125;
    if(wait_request(channel,parent,deadline) || ug_channel_receive(channel,&request))return 125;
    if(request.frame.operation!=UG_READBACK_CHECK || request.frame.sequence!=2 || request.count ||
       request.frame.incarnation!=inventory.incarnation)return 125;
    struct ug_object_close closed;memcpy(&closed,request.frame.values,sizeof(closed));
    if(closed.closed_ns>UINT64_MAX-1000000000ULL)return 125;
    uint64_t expected=closed.closed_ns+1000000000ULL;
    if(expected>deadline)expected=deadline;
    if(closed.deadline_ns!=expected)return 125;
    struct ug_readback_receipt receipt;
    int result=ug_readback_inventory(inventory_fd,&closed,&receipt);int error=errno;
    memset(&response,0,sizeof(response));response.frame=request.frame;
    response.frame.operation|=UG_RESPONSE;response.frame.rights=0;
    response.frame.error=result?(error?error:EIO):0;
    if(!result) {
        uint64_t values[8]={inventory.incarnation,inventory.proof_sequence,closed.record_ordinal,
            closed.closed_ns,closed.deadline_ns,inventory.count,receipt.observed_ns,receipt.complete_passes};
        memcpy(response.frame.values,values,sizeof(values));
    }
    if(ug_channel_send(channel,&response))return 125;
    return result?125:0;
}
