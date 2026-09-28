/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#define _GNU_SOURCE
#include <assert.h>
#include <stdarg.h>
#include "keeper-session.c"
/* Every process/map/FD/filesystem operation below is link-wrapped. These call
 * the actual ug_session_terminal method; none loads a policy or uses a real FD. */
static struct ug_session subject;
static struct ug_status status_value;
static struct ug_birth birth_value;
static struct ug_initial_task initial_value;
static bool pins[UG_MAPS+UG_LINKS], links[UG_LINKS], dir_present;
static int live_pidfd, hash_entries, unlink_error, wrong_pin, external_link;
static unsigned unlinks, destroys, locked_reads, writes;
static bool object_closed;
static int slot_name(const char *name) {
    unsigned n=0;assert(sscanf(name+1,"%u",&n)==1);
    if(name[0]=='m') {assert(n<UG_MAPS);return (int)n;}
    assert(name[0]=='l' && n<UG_LINKS);return UG_MAPS+(int)n;
}
long __wrap_syscall(long nr,...) {
    assert(nr==SYS_bpf);va_list ap;va_start(ap,nr);
    int cmd=va_arg(ap,int);union bpf_attr *a=va_arg(ap,union bpf_attr *);
    assert(va_arg(ap,size_t)==sizeof(*a));va_end(ap);
    switch(cmd) {
    case BPF_MAP_LOOKUP_ELEM:
        if(a->map_fd==107)memcpy((void *)(uintptr_t)a->value,&birth_value,sizeof(birth_value));
        else if(a->map_fd==108)memcpy((void *)(uintptr_t)a->value,&initial_value,sizeof(initial_value));
        else {assert(a->map_fd==101 && a->flags==BPF_F_LOCK);locked_reads++;
              memcpy((void *)(uintptr_t)a->value,&status_value,sizeof(status_value));}
        return 0;
    case BPF_MAP_GET_NEXT_KEY:
        assert(a->map_fd==105 || a->map_fd==106);
        if(hash_entries)return 0;errno=ENOENT;return -1;
    case BPF_OBJ_GET: {
        assert(a->path_fd==901);int i=slot_name((const char *)(uintptr_t)a->pathname);
        if(!pins[i]) {errno=ENOENT;return -1;}return 200+i;
    }
    case BPF_OBJ_GET_INFO_BY_FD: {
        int i=(int)a->info.bpf_fd-200;assert(i>=0 && i<UG_MAPS+UG_LINKS);
        u32 id=1000+(u32)i+(wrong_pin==i+1?100:0);
        if(i<UG_MAPS)((struct bpf_map_info *)(uintptr_t)a->info.info)->id=id;
        else ((struct bpf_link_info *)(uintptr_t)a->info.info)->id=id;
        return 0;
    }
    case BPF_LINK_GET_FD_BY_ID: {
        int i=(int)a->link_id-1000-UG_MAPS;assert(i>=0 && i<UG_LINKS);
        if(links[i] || external_link==i+1)return 600+i;
        errno=ENOENT;return -1;
    }
    default:assert(!"unexpected BPF command");return -1;
    }
}
int __wrap_poll(struct pollfd *p,nfds_t n,int timeout) {
    assert(n==1 && timeout==0 && p->events==POLLIN);
    p->revents=p->fd==live_pidfd?0:POLLIN;return p->revents?1:0;
}
ssize_t __wrap_write(int fd,const void *bytes,size_t size) {
    assert(fd==900 && size==sizeof(struct recovery_record));
    const struct recovery_record *r=bytes;
    assert(r->incarnation==7 && r->ordinal==subject.next_record+1);
    writes++;return (ssize_t)size;
}
int __wrap_fdatasync(int fd) {assert(fd==900);return 0;}
int __wrap_close(int fd) {assert(fd>=200 && fd<UG_MAPS+UG_LINKS+601);return 0;}
int __wrap_unlinkat(int fd,const char *name,int flags) {
    if(flags==AT_REMOVEDIR) {assert(fd==902 && !strcmp(name,"ugb1-test"));dir_present=false;return 0;}
    assert(fd==901 && flags==0);int i=slot_name(name);
    if(unlink_error==i+1) {errno=EACCES;return -1;}
    assert(pins[i]);pins[i]=false;unlinks++;return 0;
}
int __wrap_fstat(int fd,struct stat *st) {
    assert(fd==901);memset(st,0,sizeof(*st));st->st_mode=S_IFDIR;
    st->st_dev=subject.directory_dev;st->st_ino=subject.directory_ino;return 0;
}
int __wrap_fstatat(int fd,const char *name,struct stat *st,int flags) {
    assert(fd==902 && !strcmp(name,"ugb1-test") && flags==AT_SYMLINK_NOFOLLOW);
    if(dir_present) {memset(st,0,sizeof(*st));st->st_mode=S_IFDIR;
        st->st_dev=subject.directory_dev;st->st_ino=subject.directory_ino;return 0;}
    errno=ENOENT;return -1;
}
int bpf_link__destroy(struct bpf_link *p) {
    int i=(int)(uintptr_t)p-1;assert(i>=0 && i<UG_LINKS && links[i]);
    links[i]=false;destroys++;return 0;
}
void bpf_object__close(struct bpf_object *p) {assert(p==(void *)123);object_closed=true;}
static void fresh(void) {
    memset(&subject,0,sizeof(subject));memset(&status_value,0,sizeof(status_value));
    subject.record=900;subject.pin_dir=901;subject.pin_root=902;strcpy(subject.directory_name,"ugb1-test");
    subject.directory_dev=17;subject.directory_ino=19;
    subject.incarnation=7;subject.creator=30;subject.creator_sequence=9;subject.controller=31;
    subject.prepared=true;subject.object=(void *)123;subject.links_count=UG_LINKS;
    subject.initial_count=1;subject.initial[0]=(struct initial_owner){32,2,true};
    for(u32 i=0;i<UG_MAPS;i++)subject.maps[i]=100+(int)i;
    for(u32 i=0;i<UG_MAPS+UG_LINKS;i++) {subject.pin_ids[i]=1000+i;pins[i]=true;}
    for(u32 i=0;i<UG_LINKS;i++) {subject.links[i]=(void *)(uintptr_t)(i+1);links[i]=true;}
    birth_value=(struct ug_birth){7,9,UG_BIRTH_COMMITTED,0,100,101,102};
    initial_value=(struct ug_initial_task){7,UG_INITIAL_TERMINAL};
    live_pidfd=hash_entries=unlink_error=wrong_pin=external_link=0;
    unlinks=destroys=locked_reads=writes=0;object_closed=false;dir_present=true;
}
static void refuses_without_unpin(int expected) {
    struct ug_terminal_receipt receipt;
    assert(ug_session_terminal(&subject,10,&receipt)==-1 && errno==expected);
    assert(subject.admissions_closed && !subject.terminal_proved && unlinks==0 && destroys==0);
    assert(!object_closed && dir_present);
}
static void completes(void) {
    struct ug_terminal_receipt receipt;
    int terminal=ug_session_terminal(&subject,10,&receipt);
    if(terminal)fprintf(stderr,"terminal control failed: errno=%d\n",errno);
    assert(terminal==0);
    assert(receipt.incarnation==7 && receipt.sequence==10 && receipt.record_ordinal==subject.next_record);
    assert(receipt.removed_links==31 && receipt.removed_map_pins==10 && receipt.initial_tasks==subject.initial_count);
    assert(subject.terminal_proved && subject.released && destroys==31 && unlinks==41);
    assert(!dir_present && object_closed);
    for(u32 i=0;i<UG_MAPS+UG_LINKS;i++)assert(!pins[i]);
}
int main(void) {
    unsigned passed=0;
    fresh();completes();assert(locked_reads==1);passed++;
    fresh();status_value.first_outcome=UG_EVENT_DENIAL;
        status_value.first_denial=(struct ug_denial){.phase=2,.incarnation=7,.reason=UG_FOREIGN_PEER};completes();
        assert(subject.terminal.first_outcome==UG_EVENT_DENIAL);passed++;
    fresh();live_pidfd=31;refuses_without_unpin(EAGAIN);passed++;
    fresh();birth_value.phase=UG_BIRTH_ARMED;refuses_without_unpin(EBUSY);passed++;
    fresh();birth_value.incarnation=8;refuses_without_unpin(EBUSY);passed++;
    fresh();birth_value.in_copy=1;refuses_without_unpin(EBUSY);passed++;
    fresh();birth_value.cookie=0;refuses_without_unpin(EBUSY);passed++;
    fresh();initial_value.phase=UG_INITIAL_LIVE;refuses_without_unpin(EAGAIN);passed++;
    fresh();initial_value.incarnation=8;refuses_without_unpin(EPROTO);passed++;
    fresh();live_pidfd=32;refuses_without_unpin(EAGAIN);passed++;
    fresh();subject.initial[0].live=false;refuses_without_unpin(EPROTO);passed++;
    fresh();status_value.live_sockets=1;refuses_without_unpin(EAGAIN);passed++;
    fresh();status_value.live_descendants=1;refuses_without_unpin(EAGAIN);passed++;
    fresh();status_value.live_namespaces=1;refuses_without_unpin(EAGAIN);passed++;
    fresh();status_value.faults=UG_MAP_FAILURE;refuses_without_unpin(EPROTO);passed++;
    fresh();hash_entries=1;refuses_without_unpin(EAGAIN);passed++;
    fresh();wrong_pin=UG_MAPS+1;struct ug_terminal_receipt r;
        assert(ug_session_terminal(&subject,10,&r)==-1 && errno==EPROTO);
        assert(subject.terminal_proved && !unlinks && !destroys && dir_present);passed++;
    fresh();unlink_error=UG_MAPS+1;
        assert(ug_session_terminal(&subject,10,&r)==-1 && errno==EACCES);
        assert(subject.terminal_proved && !unlinks && !destroys);
        unlink_error=0;completes();passed++;
    fresh();external_link=1;
        assert(ug_session_terminal(&subject,10,&r)==-1 && errno==EAGAIN);
        assert(subject.terminal_proved && unlinks==1 && destroys==1 && !object_closed);
        external_link=0;completes();passed++;
    fresh();subject.creator=subject.controller=-1;subject.initial_count=0;
        completes();assert(locked_reads==0);passed++;
    printf("guard_terminal_controls=%u passed\n",passed);assert(passed==20);return 0;
}
