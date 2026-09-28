/* SPDX-License-Identifier: GPL-2.0 */
#define _GNU_SOURCE
#include "keeper-session.h"
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <poll.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <time.h>
#include <linux/memfd.h>

struct bpf_object; struct bpf_program; struct bpf_map; struct bpf_link;
extern struct bpf_object *bpf_object__open_file(const char *, const void *);
extern long libbpf_get_error(const void *);
extern int bpf_object__load(struct bpf_object *);
extern struct bpf_program *bpf_object__next_program(const struct bpf_object *,struct bpf_program *);
extern const char *bpf_program__section_name(const struct bpf_program *);
extern struct bpf_map *bpf_object__next_map(const struct bpf_object *,const struct bpf_map *);
extern const char *bpf_map__name(const struct bpf_map *);
extern int bpf_map__fd(const struct bpf_map *);
extern int bpf_program__fd(const struct bpf_program *);
extern struct bpf_link *bpf_program__attach(const struct bpf_program *);
extern int bpf_link__fd(const struct bpf_link *);
/* Only the synchronous terminal path below may release these actual handles,
 * after durable aggregate proof. Unknown proof retains every policy pin. */
extern int bpf_link__destroy(struct bpf_link *);
extern void bpf_object__close(struct bpf_object *);

enum record_phase { RECORD_BEGIN=1, RECORD_PIN_INTENT, RECORD_PINNED,
    RECORD_PREPARED, RECORD_ARM_INTENT, RECORD_ARMED, RECORD_BIRTH,
    RECORD_INITIAL_INTENT, RECORD_INITIAL_LIVE, RECORD_FAILURE,
    RECORD_CONTROLLER, RECORD_ADMISSION_CLOSED, RECORD_TERMINAL,
    RECORD_UNPIN_INTENT, RECORD_UNPINNED, RECORD_LINK_RELEASED, RECORD_RELEASED, RECORD_ORIGINAL_ID,
    RECORD_LINK_CLOSED, RECORD_READY_TO_CLOSE, RECORD_OBJECT_CLOSED, RECORD_QUERY_DEADLINE };
struct recovery_record {
    u64 magic, incarnation, ordinal, sequence, directory_dev, directory_ino;
    u32 abi, phase, kind, id;
    s32 error; u32 reserved;
    char pin_name[32];
};
struct initial_owner { int pidfd; u64 sequence; bool live; };
struct ug_session {
    struct bpf_object *object;
    struct bpf_link *links[UG_LINKS];
    int maps[UG_MAPS], pin_dir, pin_root, record, elf, creator, controller;
    char directory_name[32];
    u32 pin_ids[UG_MAPS+UG_LINKS];
    bool unpin_started[UG_MAPS+UG_LINKS], unpinned[UG_MAPS+UG_LINKS];
    bool admissions_closed, terminal_proved, released;
    bool inventory_complete, close_prepared, close_submitted;
    int close_error;
    struct ug_inventory inventory;
    struct ug_object_close closed;
    int journal_error; /* First uncertain write/sync; never reset by a retry. */
    struct ug_terminal_receipt terminal;
    u64 incarnation, next_record, creator_sequence;
    dev_t directory_dev; ino_t directory_ino;
    u32 links_count, initial_count;
    struct initial_owner initial[UG_MAX_INITIAL_TASKS];
    bool prepared, creator_armed, failed;
};
static const char *const map_names[UG_MAPS] = {
    "ug_config","ug_status","ug_allocator","ug_events","ug_tasks",
    "ug_sockets","ug_namespaces","ug_births","ug_initial_tasks","ug_probes"
};
static const char *const sections[UG_LINKS] = {
    "fentry/copy_net_ns","fentry/setup_net","fexit/setup_net","fexit/copy_net_ns",
    "fentry/__put_net","lsm/socket_post_create","lsm/task_alloc","lsm/task_free",
    "fentry/__sk_free","lsm/unix_find","lsm/unix_stream_connect","lsm/unix_may_send",
    "lsm/socket_socketpair","lsm/socket_bind","lsm/socket_connect","lsm/socket_listen",
    "lsm/socket_accept","lsm/socket_sendmsg","lsm/socket_recvmsg","lsm/socket_getsockname",
    "lsm/socket_getpeername","lsm/socket_getsockopt","lsm/socket_setsockopt","lsm/socket_shutdown",
    "lsm/file_permission","lsm/file_ioctl","lsm/file_ioctl_compat","lsm/file_fcntl",
    "lsm/file_receive","fentry/unix_poll","fentry/unix_dgram_poll"
};
static int fail(int error) { errno=error; return -1; }
static int bpf_call(enum bpf_cmd cmd, union bpf_attr *a) {
    return (int)syscall(SYS_bpf,cmd,a,sizeof(*a));
}
static int lookup_value(int fd,const void *key,void *value,u64 flags) {
    union bpf_attr a={0};a.map_fd=fd;a.key=(u64)(uintptr_t)key;
    a.value=(u64)(uintptr_t)value;a.flags=flags;
    return bpf_call(BPF_MAP_LOOKUP_ELEM,&a);
}
static int update_value(int fd,const void *key,const void *value,u64 flags) {
    union bpf_attr a={0};a.map_fd=fd;a.key=(u64)(uintptr_t)key;
    a.value=(u64)(uintptr_t)value;a.flags=flags;
    return bpf_call(BPF_MAP_UPDATE_ELEM,&a);
}
static int poison_journal(struct ug_session *s,int error) {
    if(!s->journal_error)s->journal_error=error?error:EIO;
    s->failed=true;
    return fail(s->journal_error);
}
static int append(struct ug_session *s,u32 phase,u64 seq,u32 kind,u32 id,
                  const char *name,int error) {
    if(s->journal_error)return fail(s->journal_error);
    if(s->record<0 || s->next_record==UINT64_MAX)return fail(EOVERFLOW);
    struct recovery_record r={.magic=0x554750494E303031ULL,.incarnation=s->incarnation,
        .ordinal=s->next_record+1,.sequence=seq,.directory_dev=s->directory_dev,
        .directory_ino=s->directory_ino,.abi=UG_ABI_VERSION,.phase=phase,
        .kind=kind,.id=id,.error=error};
    if(name) { if(strlen(name)>=sizeof(r.pin_name))return fail(EINVAL);strcpy(r.pin_name,name); }
    /* The owned exclusive record has one writer. Any failed write or sync
     * leaves completion unknown: retain its prefix and stop this session's
     * journal permanently. Re-appending the same ordinal could duplicate a
     * complete unsynced row or continue after a corrupt partial row. */
    const char *bytes=(const char *)&r;size_t left=sizeof(r);
    while(left) {
        ssize_t n=write(s->record,bytes,left);
        if(n<0 && errno==EINTR)continue;
        if(n<=0)return poison_journal(s,n<0?errno:EIO);
        bytes+=n;left-=(size_t)n;
    }
    if(fdatasync(s->record))return poison_journal(s,errno);
    s->next_record++;return 0;
}
static int object_id(int fd,u32 kind,u32 *id) {
    union { struct bpf_map_info map;struct bpf_link_info link; } info={0};
    union bpf_attr a={0};a.info.bpf_fd=fd;a.info.info=(u64)(uintptr_t)&info;
    a.info.info_len=kind==0?sizeof(info.map):sizeof(info.link);
    if(bpf_call(BPF_OBJ_GET_INFO_BY_FD,&a))return -1;
    *id=kind==0?info.map.id:info.link.id;
    return *id?0:fail(EPROTO);
}
static int open_pin(struct ug_session *s,const char *name,u32 extra_flags) {
    union bpf_attr a={0};a.pathname=(u64)(uintptr_t)name;
    a.file_flags=BPF_F_PATH_FD|extra_flags;a.path_fd=s->pin_dir;
    return bpf_call(BPF_OBJ_GET,&a);
}
static int pin(struct ug_session *s,int fd,u32 kind,u32 slot) {
    char name[32];snprintf(name,sizeof(name),"%c%02u",kind?'l':'m',slot);
    u32 actual=0;if(object_id(fd,kind,&actual))return -1;
    s->pin_ids[kind?UG_MAPS+slot:slot]=actual;
    /* Intent including exact kind/id precedes pin; a crash between pin and
     * readback leaves an explicitly unresolved, discoverable pin name. */
    if(append(s,RECORD_PIN_INTENT,0,kind,actual,name,0))return -1;
    union bpf_attr a={0};a.bpf_fd=fd;a.pathname=(u64)(uintptr_t)name;
    a.file_flags=BPF_F_PATH_FD;a.path_fd=s->pin_dir;
    if(bpf_call(BPF_OBJ_PIN,&a))return -1;
    int reopened=open_pin(s,name,0);if(reopened<0)return -1;
    u32 observed=0;int result=object_id(reopened,kind,&observed);int error=errno;
    /* This closes only a new BPF object description. The verified pin and
     * session's original remain; no socket or pidfd is closed here. */
    if(close(reopened) && !result) {result=-1;error=errno;}
    if(result)return fail(error);
    if(actual!=observed)return fail(EPROTO);
    return append(s,RECORD_PINNED,0,kind,actual,name,0);
}
static int fresh_directory(int root,const char *name,mode_t mode) {
    if(mkdirat(root,name,mode))return -1;
    return openat(root,name,O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW);
}
static int check_clean(struct ug_session *s) {
    struct ug_status status;u32 zero=0;
    if(lookup_value(s->maps[1],&zero,&status,BPF_F_LOCK))return -1;
    return (!status.faults && !status.first_outcome && !status.first_denial.phase)?0:fail(ECANCELED);
}
int ug_session_open(int elf,int bpffs_root,int recovery_root,u64 incarnation,
                    struct ug_session **out) {
    if(!out || !incarnation || elf<0 || bpffs_root<0 || recovery_root<0)return fail(EINVAL);
    *out=NULL;struct ug_session *s=calloc(1,sizeof(*s));if(!s)return -1;
    *out=s;s->pin_dir=s->pin_root=s->record=s->elf=s->creator=s->controller=-1;s->incarnation=incarnation;
    for(u32 i=0;i<UG_MAPS;i++)s->maps[i]=-1;
    for(u32 i=0;i<UG_MAX_INITIAL_TASKS;i++)s->initial[i].pidfd=-1;
    struct statfs fs;struct stat roots[2];
    if(fstatfs(bpffs_root,&fs) || fstat(bpffs_root,&roots[0]) || fstat(recovery_root,&roots[1]))return -1;
    if(fs.f_type!=BPF_FS_MAGIC || !S_ISDIR(roots[0].st_mode) || !S_ISDIR(roots[1].st_mode))return fail(EINVAL);
    /* These roots must be externally owned private locations. Never adopt an
     * existing run directory or follow a preexisting leaf. */
    char name[32];snprintf(name,sizeof(name),"ugb1-%016llx",(unsigned long long)incarnation);
    strcpy(s->directory_name,name);
    s->pin_root=fcntl(bpffs_root,F_DUPFD_CLOEXEC,3);if(s->pin_root<0)return -1;
    s->record=openat(recovery_root,name,O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC|O_NOFOLLOW,0600);
    if(s->record<0 || fsync(recovery_root))return -1;
    s->pin_dir=fresh_directory(bpffs_root,name,0700);if(s->pin_dir<0)return -1;
    struct stat st;if(fstat(s->pin_dir,&st))return -1;
    s->directory_dev=st.st_dev;s->directory_ino=st.st_ino;
    if(append(s,RECORD_BEGIN,0,0,0,NULL,0))return -1;
    s->elf=fcntl(elf,F_DUPFD_CLOEXEC,3);if(s->elf<0)return -1;
    char path[64];snprintf(path,sizeof(path),"/proc/self/fd/%d",s->elf);
    s->object=bpf_object__open_file(path,NULL);long error=libbpf_get_error(s->object);
    if(!s->object || error) {s->object=NULL;return fail(error?(int)-error:EINVAL);}
    /* Reject unknown/missing/duplicate sections and map names before load. */
    struct bpf_program *programs[UG_LINKS]={0},*p=NULL;u32 count=0;
    while((p=bpf_object__next_program(s->object,p))) {
        u32 i=0;const char *section=bpf_program__section_name(p);
        for(;i<UG_LINKS;i++)if(section && !strcmp(section,sections[i]))break;
        if(i==UG_LINKS || programs[i])return fail(EPROTO);
        programs[i]=p;count++;
    }
    if(count!=UG_LINKS)return fail(EPROTO);
    struct bpf_map *maps[UG_MAPS]={0},*m=NULL;count=0;
    while((m=bpf_object__next_map(s->object,m))) {
        u32 i=0;const char *name=bpf_map__name(m);
        for(;i<UG_MAPS;i++)if(name && !strcmp(name,map_names[i]))break;
        if(i==UG_MAPS || maps[i])return fail(EPROTO);
        maps[i]=m;count++;
    }
    if(count!=UG_MAPS || bpf_object__load(s->object))return count!=UG_MAPS?fail(EPROTO):-1;
    for(u32 i=0;i<UG_MAPS;i++) {s->maps[i]=bpf_map__fd(maps[i]);if(s->maps[i]<0)return fail(EPROTO);}
    u32 zero=0;struct ug_config config={incarnation,UG_DENY,UG_ABI_VERSION};
    if(update_value(s->maps[0],&zero,&config,BPF_ANY))return -1;
    union bpf_attr freeze={0};freeze.map_fd=s->maps[0];
    if(bpf_call(BPF_MAP_FREEZE,&freeze))return -1;
    for(u32 i=0;i<UG_MAPS;i++)if(pin(s,s->maps[i],0,i))return -1;
    for(u32 i=0;i<UG_LINKS;i++) {
        struct bpf_link *link=bpf_program__attach(programs[i]);error=libbpf_get_error(link);
        if(!link || error)return fail(error?(int)-error:EINVAL);
        s->links[s->links_count++]=link;
        if(pin(s,bpf_link__fd(link),1,i))return -1;
    }
    if(check_clean(s) || append(s,RECORD_PREPARED,0,0,0,NULL,0))return -1;
    s->prepared=true;return 0;
}
int ug_session_readers(struct ug_session *s,int out[3]) {
    if(!s || !out || !s->prepared)return fail(EINVAL);
    const u32 indexes[3]={0,1,3};
    for(u32 i=0;i<3;i++) {
        char name[32];snprintf(name,sizeof(name),"m%02u",indexes[i]);
        out[i]=open_pin(s,name,BPF_F_RDONLY);if(out[i]<0)return -1;
        int flags=fcntl(out[i],F_GETFD);if(flags<0 || fcntl(out[i],F_SETFD,flags|FD_CLOEXEC))return -1;
        u32 a=0,b=0;if(object_id(out[i],0,&a) || object_id(s->maps[indexes[i]],0,&b))return -1;
        if(a!=b)return fail(EPROTO);
    }
    return 0;
}
int ug_session_creator_recovery(struct ug_session *s,int *out) {
    if(!s || !out || !s->prepared || s->failed || s->creator>=0)return fail(EINVAL);
    *out=open_pin(s,"m07",0);if(*out<0)return -1;
    /* Caller owns *out even on a later verification error. */
    int flags=fcntl(*out,F_GETFD);if(flags<0 || fcntl(*out,F_SETFD,flags|FD_CLOEXEC))return -1;
    u32 expected=0,actual=0;
    if(object_id(s->maps[7],0,&expected) || object_id(*out,0,&actual))return -1;
    return actual==expected?0:fail(EPROTO);
}
int ug_session_arm_creator(struct ug_session *s,int pidfd,u64 sequence) {
    if(!s || !s->prepared || s->failed || s->admissions_closed || !sequence || s->creator>=0 || pidfd<0)return fail(EINVAL);
    s->creator=fcntl(pidfd,F_DUPFD_CLOEXEC,3);if(s->creator<0)return -1;
    s->creator_sequence=sequence;
    if(append(s,RECORD_ARM_INTENT,sequence,0,0,NULL,0))return -1;
    struct ug_birth command={.incarnation=s->incarnation,.sequence=sequence,.phase=UG_BIRTH_ARMED};
    if(update_value(s->maps[7],&s->creator,&command,BPF_NOEXIST))return -1;
    struct ug_birth observed;
    if(lookup_value(s->maps[7],&s->creator,&observed,0))return -1;
    if(memcmp(&command,&observed,sizeof(command)) || check_clean(s))return fail(EPROTO);
    if(append(s,RECORD_ARMED,sequence,0,0,NULL,0))return -1;
    s->creator_armed=true;return 0;
}
int ug_session_observe_birth(struct ug_session *s,u64 sequence,struct ug_birth *out) {
    if(!s || !out || !s->creator_armed || sequence!=s->creator_sequence)return fail(EINVAL);
    if(lookup_value(s->maps[7],&s->creator,out,0) || check_clean(s))return -1;
    if(out->incarnation!=s->incarnation || out->sequence!=sequence ||
       out->phase!=UG_BIRTH_COMMITTED || out->in_copy || !out->object || !out->generation || !out->cookie)
        return fail(ENODATA);
    return append(s,RECORD_BIRTH,sequence,0,0,NULL,0);
}
int ug_session_register_initial(struct ug_session *s,int pidfd,u64 sequence) {
    if(!s || !s->prepared || s->failed || s->admissions_closed || pidfd<0 || !sequence ||
       s->initial_count==UG_MAX_INITIAL_TASKS)return fail(EINVAL);
    struct initial_owner *owner=&s->initial[s->initial_count++];owner->sequence=sequence;
    owner->pidfd=fcntl(pidfd,F_DUPFD_CLOEXEC,3);if(owner->pidfd<0)return -1;
    if(append(s,RECORD_INITIAL_INTENT,sequence,0,0,NULL,0))return -1;
    struct ug_initial_task initial={s->incarnation,UG_INITIAL_STAGED};
    if(update_value(s->maps[8],&sequence,&initial,BPF_NOEXIST))return -1;
    struct ug_task task={.incarnation=s->incarnation,.initial_registration=sequence};
    if(update_value(s->maps[4],&owner->pidfd,&task,BPF_NOEXIST))return -1;
    initial.phase=UG_INITIAL_LIVE;
    if(update_value(s->maps[8],&sequence,&initial,BPF_EXIST))return -1;
    struct ug_task found;struct ug_initial_task row;
    if(lookup_value(s->maps[4],&owner->pidfd,&found,0) || lookup_value(s->maps[8],&sequence,&row,0) ||
       memcmp(&found,&task,sizeof(task)) || !ug_initial_membership_matches(&found,&row) || check_clean(s))return fail(EPROTO);
    /* A task killed during registration leaves a fault/partial row retained.
     * The caller must separately keep its authentic ptrace stop until ACK. */
    if(append(s,RECORD_INITIAL_LIVE,sequence,0,0,NULL,0))return -1;
    owner->live=true;return 0;
}
int ug_session_monitor(struct ug_session *s,int other,struct ug_monitor_result *result) {
    if(!s || !s->prepared)return fail(EINVAL);
    struct ug_monitor_fds f={s->maps[0],s->maps[1],s->maps[3],other,s->incarnation};
    return ug_monitor_once(&f,result);
}
int ug_session_note_failure(struct ug_session *s,u64 seq,int error) {
    if(!s)return fail(EINVAL);s->failed=true;
    return append(s,RECORD_FAILURE,seq,0,0,NULL,error);
}

static int task_terminal(int pidfd) {
    if(pidfd<0)return fail(EINVAL);
    struct pollfd p={.fd=pidfd,.events=POLLIN};
    int n=poll(&p,1,0);if(n<0)return -1;
    if(p.revents&(POLLERR|POLLNVAL))return fail(EPROTO);
    return n>0 && (p.revents&POLLIN)?0:fail(EAGAIN);
}
int ug_session_bind_controller(struct ug_session *s,int pidfd,u64 sequence) {
    if(!s || !s->prepared || s->failed || s->admissions_closed ||
       s->controller>=0 || s->initial_count || pidfd<0 || !sequence)return fail(EINVAL);
    s->controller=fcntl(pidfd,F_DUPFD_CLOEXEC,3);if(s->controller<0)return -1;
    return append(s,RECORD_CONTROLLER,sequence,0,0,NULL,0);
}
static int empty_hash(int map) {
    u64 key=0;union bpf_attr a={0};a.map_fd=map;a.next_key=(u64)(uintptr_t)&key;
    if(!bpf_call(BPF_MAP_GET_NEXT_KEY,&a))return fail(EAGAIN);
    return errno==ENOENT?0:-1;
}
static int prove_terminal(struct ug_session *s) {
    if(s->controller>=0 && task_terminal(s->controller))return -1;
    /* No registration/arm method runs after admissions_closed. Before any
     * admission attempt, even a partial load has no cohort or owned namespace. */
    if(s->creator<0 && !s->initial_count)return 0;
    if(s->maps[7]<0 || s->maps[8]<0 || s->maps[1]<0)return fail(EPROTO);
    if(s->creator>=0) {
        struct ug_birth b;
        if(lookup_value(s->maps[7],&s->creator,&b,0)) {
            if(errno!=ENOENT)return -1;
            /* Exact outside recovery may have deleted only the unused arm.
             * Controller death alone never authorizes an ARMED command. */
        } else if(b.incarnation!=s->incarnation || b.sequence!=s->creator_sequence || b.in_copy ||
                  (b.phase!=UG_BIRTH_COMMITTED && b.phase!=UG_BIRTH_FAILED) ||
                  (b.phase==UG_BIRTH_COMMITTED && (!b.object || !b.generation || !b.cookie)))
            return fail(EBUSY);
    }
    for(u32 i=0;i<s->initial_count;i++) {
        struct initial_owner *o=&s->initial[i];struct ug_initial_task row;
        if(o->pidfd<0 || !o->live)return fail(EPROTO);
        if(task_terminal(o->pidfd))return -1;
        if(lookup_value(s->maps[8],&o->sequence,&row,0))return -1;
        if(row.incarnation!=s->incarnation)return fail(EPROTO);
        if(row.phase!=UG_INITIAL_TERMINAL)return fail(EAGAIN);
    }
    struct ug_status status;u32 zero=0;
    if(lookup_value(s->maps[1],&zero,&status,BPF_F_LOCK))return -1;
    /* A tracking fault makes a zero count insufficient. Never clear faults,
     * queues, membership, or sockets to manufacture a terminal certificate. */
    if(status.faults || status.first_outcome>UG_EVENT_INTERNAL)return fail(EPROTO);
    if(!status.first_outcome && status.first_denial.phase)return fail(EAGAIN);
    if(status.first_outcome==UG_EVENT_DENIAL &&
       (status.first_denial.phase!=2 || status.first_denial.incarnation!=s->incarnation))return fail(EPROTO);
    if(status.live_sockets || status.live_descendants || status.live_namespaces)return fail(EAGAIN);
    if(empty_hash(s->maps[5]) || empty_hash(s->maps[6]))return -1;
    s->terminal.first_outcome=status.first_outcome;
    s->terminal.guard_faults=status.faults;
    return 0;
}
static int unpin_exact(struct ug_session *s,u32 kind,u32 slot,u64 sequence) {
    u32 index=kind?UG_MAPS+slot:slot,expected=s->pin_ids[index];
    if(!expected || s->unpinned[index])return 0;
    char name[32];snprintf(name,sizeof(name),"%c%02u",kind?'l':'m',slot);
    int fd=open_pin(s,name,0);
    if(fd>=0) {
        u32 actual=0;int result=object_id(fd,kind,&actual),saved=errno;
        if(close(fd) && !result) {result=-1;saved=errno;}
        if(result)return fail(saved);
        if(actual!=expected)return fail(EPROTO);
        if(!s->unpin_started[index]) {
            if(append(s,RECORD_UNPIN_INTENT,sequence,kind,expected,name,0))return -1;
            s->unpin_started[index]=true;
        }
        if(unlinkat(s->pin_dir,name,0))return -1;
    } else if(errno!=ENOENT)return -1;
    fd=open_pin(s,name,0);
    if(fd>=0) {close(fd);return fail(EBUSY);}
    if(errno!=ENOENT)return -1;
    if(append(s,RECORD_UNPINNED,sequence,kind,expected,name,0))return -1;
    s->unpinned[index]=true;return 0;
}
static int link_absent(u32 id) {
    union bpf_attr a={0};a.link_id=id;
    int fd=bpf_call(BPF_LINK_GET_FD_BY_ID,&a);
    if(fd>=0) {int result=close(fd);if(result)return -1;return fail(EAGAIN);}
    return errno==ENOENT?0:-1;
}
/* The pin root is exclusively owned. Verify the current leaf and retained
 * descriptor against the recorded directory before removing that leaf. This
 * is positive ownership readback, not atomic protection against a concurrent
 * same-UID rename outside that exclusive-root contract. */
static int remove_owned_directory(struct ug_session *s) {
    struct stat held,leaf;
    if(fstat(s->pin_dir,&held))return -1;
    if(!S_ISDIR(held.st_mode) || held.st_dev!=s->directory_dev ||
       held.st_ino!=s->directory_ino)return fail(ESTALE);
    if(!fstatat(s->pin_root,s->directory_name,&leaf,AT_SYMLINK_NOFOLLOW)) {
        if(!S_ISDIR(leaf.st_mode) || leaf.st_dev!=held.st_dev ||
           leaf.st_ino!=held.st_ino)return fail(ESTALE);
        if(unlinkat(s->pin_root,s->directory_name,AT_REMOVEDIR))return -1;
    } else if(errno!=ENOENT)return -1;
    /* An already absent leaf requires no removal; a replacement is never
     * removed. Preserve the original post-removal absence check. */
    if(!fstatat(s->pin_root,s->directory_name,&leaf,AT_SYMLINK_NOFOLLOW))return fail(EBUSY);
    return errno==ENOENT?0:-1;
}
static int terminal_core(struct ug_session *s,u64 sequence,struct ug_terminal_receipt *out,bool defer_close) {
    if(!s || !out || !sequence || s->record<0)return fail(EINVAL);
    memset(out,0,sizeof(*out));
    if(s->journal_error)return fail(s->journal_error);
    if(!s->admissions_closed) {
        /* The actual command lane is serial; close before any terminal read. */
        s->admissions_closed=true;
        if(append(s,RECORD_ADMISSION_CLOSED,sequence,0,0,NULL,0))return -1;
    }
    if(!s->terminal_proved) {
        if(prove_terminal(s))return -1;
        if(append(s,RECORD_TERMINAL,sequence,0,0,NULL,0))return -1;
        s->terminal_proved=true;
        s->terminal.incarnation=s->incarnation;s->terminal.sequence=sequence;
        s->terminal.initial_tasks=s->initial_count;
    }
    if(!s->released) {
        for(u32 i=0;i<s->links_count;i++) {
            if(unpin_exact(s,1,i,sequence))return -1;
            if(s->links[i]) {
                struct bpf_link *link=s->links[i];s->links[i]=NULL;
                int result=bpf_link__destroy(link);
                if(result)return fail(result<0?-result:result);
            }
            if(!defer_close && link_absent(s->pin_ids[UG_MAPS+i]))return -1;
            if(append(s,defer_close?RECORD_LINK_CLOSED:RECORD_LINK_RELEASED,sequence,1,s->pin_ids[UG_MAPS+i],NULL,0))return -1;
        }
        for(u32 i=0;i<UG_MAPS;i++)if(unpin_exact(s,0,i,sequence))return -1;
        if(defer_close) {
            if(!s->close_prepared) {
                if(append(s,RECORD_READY_TO_CLOSE,sequence,0,0,NULL,0))return -1;
                s->close_prepared=true;
                s->terminal.removed_links=s->links_count;
                s->terminal.removed_map_pins=0;
                for(u32 i=0;i<UG_MAPS;i++)if(s->pin_ids[i])s->terminal.removed_map_pins++;
                s->terminal.record_ordinal=s->next_record;
            }
            *out=s->terminal;return 0;
        }
        if(s->object) {bpf_object__close(s->object);s->object=NULL;}
        for(u32 i=0;i<UG_MAPS;i++)s->maps[i]=-1;
        if(s->pin_dir>=0 && remove_owned_directory(s))return -1;
        if(append(s,RECORD_RELEASED,sequence,0,0,NULL,0))return -1;
        s->terminal.removed_links=s->links_count;
        for(u32 i=0;i<UG_MAPS;i++)if(s->pin_ids[i])s->terminal.removed_map_pins++;
        s->terminal.record_ordinal=s->next_record;s->released=true;
    }
    *out=s->terminal;return 0;
}

int ug_session_terminal(struct ug_session *s,u64 sequence,struct ug_terminal_receipt *out) {
    return terminal_core(s,sequence,out,false);
}
static int original_id(struct ug_session *s,int fd,u32 kind,u64 sequence) {
    if(fd<0)return 0;
    union {struct bpf_map_info map;struct bpf_prog_info program;struct bpf_link_info link;} info={0};
    union bpf_attr a={0};a.info.bpf_fd=fd;a.info.info=(u64)(uintptr_t)&info;
    a.info.info_len=kind==0?sizeof(info.map):kind==1?sizeof(info.program):sizeof(info.link);
    if(bpf_call(BPF_OBJ_GET_INFO_BY_FD,&a))return -1;
    u32 id=kind==0?info.map.id:kind==1?info.program.id:info.link.id;
    if(!id || kind>2 || s->inventory.count>=UG_INVENTORY_MAX)return fail(EPROTO);
    for(u32 i=0;i<s->inventory.count;i++)
        if(s->inventory.ids[i].kind==kind && s->inventory.ids[i].id==id)return 0;
    if(append(s,RECORD_ORIGINAL_ID,sequence,kind,id,NULL,0))return -1;
    s->inventory.ids[s->inventory.count++]=(struct ug_plain_id){kind,id};
    if(kind==0)s->inventory.maps++;else if(kind==1)s->inventory.programs++;else s->inventory.links++;
    return 0;
}
static int capture_inventory(struct ug_session *s,u64 sequence) {
    if(s->inventory_complete)return 0;
    if(s->journal_error)return fail(s->journal_error);
    if(s->object) {
        struct bpf_map *map=NULL;
        while((map=bpf_object__next_map(s->object,map)))
            if(original_id(s,bpf_map__fd(map),0,sequence))return -1;
        struct bpf_program *program=NULL;
        while((program=bpf_object__next_program(s->object,program)))
            if(original_id(s,bpf_program__fd(program),1,sequence))return -1;
    }
    for(u32 i=0;i<s->links_count;i++)
        if(s->links[i] && original_id(s,bpf_link__fd(s->links[i]),2,sequence))return -1;
    if(s->prepared && (s->inventory.maps!=UG_MAPS || s->inventory.programs!=UG_LINKS ||
                       s->inventory.links!=UG_LINKS))return fail(EPROTO);
    s->inventory.magic=UG_INVENTORY_MAGIC;s->inventory.incarnation=s->incarnation;
    s->inventory_complete=true;return 0;
}
static int export_inventory(struct ug_session *s,int *out) {
    int fd=(int)syscall(SYS_memfd_create,"hermit-unix-original-ids",MFD_CLOEXEC|MFD_ALLOW_SEALING);
    if(fd<0)return -1;
    *out=fd; /* Metadata ownership returned even on a subsequent failure. */
    const char *bytes=(const char *)&s->inventory;size_t left=sizeof(s->inventory);
    while(left) {
        ssize_t n=write(fd,bytes,left);
        if(n<0 && errno==EINTR)continue;
        if(n<=0)return n<0?-1:fail(EIO);
        bytes+=n;left-=(size_t)n;
    }
    if(fcntl(fd,F_ADD_SEALS,F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE))return -1;
    return 0;
}
int ug_session_prepare_terminal(struct ug_session *s,u64 sequence,struct ug_terminal_receipt *out,int *inventory_fd) {
    if(!s || !out || !inventory_fd || !sequence)return fail(EINVAL);
    *inventory_fd=-1;
    if(s->close_submitted)return fail(EBUSY);
    if(capture_inventory(s,sequence))return -1;
    if(!s->close_prepared && terminal_core(s,sequence,out,true))return -1;
    *out=s->terminal;
    s->inventory.proof_sequence=out->sequence;s->inventory.record_ordinal=out->record_ordinal;
    return export_inventory(s,inventory_fd);
}
static int monotonic_ns(u64 *out) {
    struct timespec now;if(clock_gettime(CLOCK_MONOTONIC,&now))return -1;
    if(now.tv_sec<0 || now.tv_nsec<0 || now.tv_nsec>=1000000000 ||
       (u64)now.tv_sec>(UINT64_MAX-(u64)now.tv_nsec)/1000000000ULL)return fail(EOVERFLOW);
    *out=(u64)now.tv_sec*1000000000ULL+(u64)now.tv_nsec;return 0;
}
int ug_session_close_terminal(struct ug_session *s,u64 sequence,u64 proof_sequence,
                              u64 proof_ordinal,u64 deadline,struct ug_object_close *out) {
    if(!s || !out || !sequence || !deadline || !s->close_prepared ||
       proof_sequence!=s->terminal.sequence || proof_ordinal!=s->terminal.record_ordinal)return fail(EINVAL);
    if(s->journal_error)return fail(s->journal_error);
    if(s->close_error)return fail(s->close_error);
    if(s->close_submitted)return fail(EBUSY); /* Unknown/replayed close is not a renewed window. */
    u64 now;if(monotonic_ns(&now))return -1;
    if(now>=deadline)return fail(ETIMEDOUT);
    s->close_submitted=true;
    if(s->object) {bpf_object__close(s->object);s->object=NULL;}
    int timed=monotonic_ns(&now),time_error=errno;
    for(u32 i=0;i<UG_MAPS;i++)s->maps[i]=-1;
    if(timed) {s->close_error=time_error?time_error:EIO;return fail(s->close_error);}
    if(now>UINT64_MAX-1000000000ULL) {s->close_error=EOVERFLOW;return fail(EOVERFLOW);}
    s->closed=(struct ug_object_close){s->incarnation,proof_sequence,0,now,
        now+1000000000ULL, s->inventory.count,s->terminal.first_outcome,s->terminal.guard_faults};
    if(s->closed.deadline_ns>deadline)s->closed.deadline_ns=deadline;
    char stamp[32];snprintf(stamp,sizeof(stamp),"%016llx",(unsigned long long)now);
    if(append(s,RECORD_OBJECT_CLOSED,sequence,0,0,stamp,0))return -1;
    snprintf(stamp,sizeof(stamp),"%016llx",(unsigned long long)s->closed.deadline_ns);
    if(append(s,RECORD_QUERY_DEADLINE,sequence,0,0,stamp,0))return -1;
    if(s->pin_dir>=0 && remove_owned_directory(s)) {s->close_error=errno;return -1;}
    s->closed.record_ordinal=s->next_record;
    *out=s->closed;return 0;
}
