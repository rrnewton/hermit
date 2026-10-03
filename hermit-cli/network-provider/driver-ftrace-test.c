/* SPDX-License-Identifier: MIT */
/* Actual FtraceV1 driver constructor, identifier walk and destructor.
 * All target files, BTF, libbpf objects and syscalls below are deterministic
 * host premises. This does NOT load BPF or establish a kernel receipt.
 * Compile and check undefined symbols as specified by the maintained-runner
 * proposal before executing. Do not link libbpf or bypass that fence. */
#include "driver-ftrace-facade.h"
#include "driver-grouped.c"

#define DF_COUNT(a) (sizeof(a)/sizeof((a)[0]))
#define DF_PID 4242
#define DF_PIDFD 900
#define DF_PROVIDER UINT64_C(0x123456789)
#define DF_ANCHOR (AP_GROUPED_CONNECT_IMAGE+UINT64_C(0x200000))
#define DF_MAPS 24U
#define DF_TOTAL (DF_MAPS+AP_PROGRAMS+AP_LINKS)
_Static_assert(AP_PROGRAMS==49 && AP_LINKS==49,"accepted-contract.json FtraceV1");
_Static_assert(DF_TOTAL==122,"three separate identifier namespaces");

static _Noreturn void df_unexpected(const char *what) {
    fprintf(stderr,"unmocked or out-of-contract driver effect: %s\n",what);
    abort();
}
enum df_fault {
    DF_OK, DF_PAGE_SIZE, DF_PAGE_STRUCT, DF_BUILD_ID, DF_IMAGE, DF_GROUP_IMAGE,
    DF_MISSING_PROGRAM, DF_EXTRA_PROGRAM, DF_DUP_PROGRAM, DF_DUP_LINK,
    DF_PERF_LINK, DF_COOKIE, DF_ADDRESS, DF_COUNT_BAD, DF_FLAGS, DF_SHORT_INFO,
    DF_MISSING_REQUIRED_MAP, DF_MISSING_MAP, DF_EXTRA_MAP, DF_DUP_MAP,
    DF_ATTACH_FAIL, DF_LOAD_FAIL, DF_CONFIG_FAIL, DF_RING_FAIL, DF_READ_LINK,
    DF_ANCHOR_FAIL, DF_WRONG_PROGRAM_KIND, DF_LEGACY_PROGRAM,
    DF_FAULT_TARGET, DF_FAULT_MISS, DF_FAULT_SHORT, DF_FAULT_MAP, DF_FAULT_MAP_MISSING,
    DF_OPEN_FAIL, DF_PROGRAM_MISS, DF_LINK_MISS
};
static enum df_fault df_fault;
static unsigned df_bad_at;
struct bpf_object { bool open,loaded;unsigned programs,maps; };
struct bpf_program { unsigned at,id,type;int fd;const char *name;bool unloaded; };
struct bpf_map { unsigned at,id,type;int fd;const char *name; };
struct bpf_link {
    unsigned at,id,program_id,type,count,flags,queries,shape_queries;
    int fd;bool alive;
    u64 addresses[AP_MEMBERSHIP_ENTRY_COUNT],cookies[AP_MEMBERSHIP_ENTRY_COUNT];
};
struct btf { bool alive; };
struct ring { unsigned unused; };
struct ring_buffer { bool alive; };
static struct bpf_object df_object;
static struct bpf_program df_programs[AP_PROGRAMS+1];
static struct bpf_map df_maps[DF_MAPS+1];
static struct bpf_link df_links[AP_LINKS+1];
static struct btf df_btf;
static struct ring_buffer df_ring;
static struct ap_session df_session;
static bool df_session_alive,df_owner_alive;
static struct ap_config df_config;
static unsigned df_attached,df_attach_calls,df_unloads,df_destroyed,df_map_closes;
static unsigned df_program_closes,df_object_closes,df_ring_closes,df_updates,df_lookups;
static unsigned df_file_opens,df_file_closes,df_image_opens,df_page_queries,df_btf_queries;
static unsigned df_anchor_calls,df_owner_closes,df_info_queries,df_cases;
static unsigned df_destroy_order[AP_LINKS];
static int df_destroy_failure;
static bool df_terminal;
static unsigned df_terminal_unloads;
static struct ap_stream_fault_state dm_fault;
static unsigned dm_fault_reads;
static bool dm_unstable,dm_info_short,dm_info_error;
static unsigned dm_global_id_queries;

/* Explicit metadata inventory transcribed from the active Ftrace object:
 * 38 fentry/fexit + 3 tp_btf + 8 multi/session programs. Names are fixture
 * inputs, not reconstructed from the driver's admission decisions. */
static const struct { const char *name;unsigned type;bool raw; } df_inventory[]={
#define T(n) {#n,BPF_PROG_TYPE_TRACING,false}
#define R(n) {#n,BPF_PROG_TYPE_TRACING,true}
#define K(n) {#n,BPF_PROG_TYPE_KPROBE,false}
    T(held_fd_probe),T(socket_set_enter),T(socket_set_exit),T(tcp_set_enter),T(tcp_set_exit),
    T(clone_enter),T(clone_exit),T(child_queued),T(socket_retired),
    T(fd_original_read_entered),T(fd_connect_returned),
    K(fd_so),K(fd_si),K(fd_s20e),K(fd_s20x),
    T(fd_listener_selected),T(fd_child_dequeued),T(fd_new_file_returned),
    T(fd_install_enter),T(fd_install_returned),T(fd_accept_returned),T(fd_removed),
    T(fd_remove_enter),T(fd_remove_returned),T(fd_replace_enter),
    T(fd_replaced_old_file),T(fd_replace_returned),K(fd_file_retired),
    T(fd_exec_enter),K(fd_exec_closed_file),T(fd_exec_returned),
    T(fd_native_clone_enter),T(fd_native_copy_process),T(fd_native_parent_chosen),
    R(fd_native_fork),T(fd_native_clone_returned),R(fd_native_syscall_returned),
    T(fd_copy_enter),T(fd_copy_returned),T(fd_table_put_enter),
    T(fd_table_put_returned),T(fd_table_retired),T(fd_enrollment_enter),
    T(fd_enrollment_returned),R(fd_epoll_ctl_syscall_entered),
    K(fd_stream_copy_protocol_enter),K(fd_stream_copy_protocol_exit),
    T(fd_stream_fault_enter),T(fd_stream_fault_exit)
#undef T
#undef R
#undef K
};
_Static_assert(DF_COUNT(df_inventory)==AP_PROGRAMS,"full modeled object");
static const char *const df_map_names[]={
    "ap_config_map","status","listeners","events","commands","objects","clones","setters","tasks",
    "fd_accepts","fd_enrollments","fd_status","fd_journal","fd_files","fd_tables","fd_calls",
    "fd_replacements","fd_install_calls","fd_removals","fd_puts","fd_copies","fd_execs",
    "stream_copy_records","stream_copy_faults"
};
_Static_assert(DF_COUNT(df_map_names)==DF_MAPS,"actual BPF map declarations");
static unsigned df_map_type(const char *name) {
    if(!strcmp(name,"stream_copy_faults"))return BPF_MAP_TYPE_ARRAY;
    if(!strcmp(name,"tasks"))return BPF_MAP_TYPE_TASK_STORAGE;
    if(!strcmp(name,"stream_copy_records"))return BPF_MAP_TYPE_RINGBUF;
    for(unsigned i=0;i<5;i++)if(!strcmp(name,df_map_names[i]))return BPF_MAP_TYPE_ARRAY;
    for(unsigned i=9;i<12;i++)if(!strcmp(name,df_map_names[i]))return BPF_MAP_TYPE_ARRAY;
    return BPF_MAP_TYPE_HASH;
}

/* Independent symbol/address fixture, including all 20 membership sites.
 * No grouped_* address helper is used to manufacture the query answers. */
static const struct {const char *name;u64 image;} df_symbols[]={
    {"__sys_accept4",0xffffffff82197db0ULL},{"security_socket_connect",0xffffffff8206c070ULL},
    {"__audit_sockaddr",0xffffffff813f0fb0ULL},{"__sys_connect",0xffffffff8206bf70ULL},
    {"f_dupfd",0xffffffff821ba4b0ULL},{"alloc_fd",0xffffffff821ba580ULL},
    {"do_epoll_ctl",0xffffffff81fb1ca0ULL},{"fdget",0xffffffff81fb0190ULL},
    {"fdget_raw",0xffffffff81faaca0ULL},{"fdget_pos",0xffffffff81faede0ULL},
    {"__fput.llvm.9531277854859994247",0xffffffff820720f0ULL},{"filp_close",0xffffffff82058c60ULL},
    {"inet_recvmsg",0xffffffff82355dc0ULL},{"inet6_recvmsg",0xffffffff820524a0ULL},
    {"unix_stream_recvmsg",0xffffffff8215a110ULL},{"skb_copy_datagram_iter",0xffffffff81fb85d0ULL},
    {"tcp_recvmsg",0xffffffff81fb7350ULL},{"tcp_recvmsg_locked",0xffffffff8206dfe0ULL},
    {"unix_stream_read_generic",0xffffffff8215a180ULL},{"tcp_splice_read",0xffffffff81e68b10ULL},
    {"tcp_read_sock",0xffffffff81e69130ULL},{"tcp_read_sock_noack",0xffffffff81e69310ULL},
    {"__tcp_read_sock",0xffffffff81e69150ULL},{"tcp_read_skb",0xffffffff81e69320ULL},
    {"tcp_read_done",0xffffffff81e69440ULL},{"tcp_zerocopy_receive",0xffffffff81e6a2e0ULL},
    {"tcp_bpf_recvmsg",0xffffffff81e9c2c0ULL},{"tcp_bpf_recvmsg_parser",0xffffffff81e9c510ULL},
    {"unix_stream_splice_read",0xffffffff81eb56d0ULL},{"unix_stream_read_skb",0xffffffff81eb5750ULL},
    {"unix_read_skb",0xffffffff81eb5df0ULL},{"unix_bpf_recvmsg",0xffffffff81eb6f10ULL},
    {"tcp_sendmsg",0xffffffff81fbabf0ULL},{"lock_sock_nested",0xffffffff81fb70c0ULL},
    {"release_sock",0xffffffff81f54820ULL}
};
static u64 df_address(const char *name) {
    for(unsigned i=0;i<DF_COUNT(df_symbols);i++)
        if(!strcmp(name,df_symbols[i].name))return df_symbols[i].image+UINT64_C(0x200000);
    df_unexpected(name);
}
static unsigned df_index(const char *name) {
    for(unsigned i=0;i<AP_PROGRAMS;i++)if(!strcmp(df_inventory[i].name,name))return i;
    df_unexpected(name);
}

/* Sparse in-memory target files. Expected image arrays are explicit fixture
 * premises; the real fopen/read/seek callers and byte comparators run. */
struct df_file { bool alive;unsigned kind;size_t cursor;const struct ap_copy_image_slice *slice; };
static struct df_file df_file;
static const char df_kallsyms[]=
    "0000000000000000 t __fput.llvm.9531277854859994247\n"
    "0000000000000000 t do_close_on_exec\n"
    "0000000000000000 t filp_close\n"
    "0000000000000000 t ptrace_request\n";
static struct df_file *df_file_checked(FILE *f) {
    assert(f==(FILE *)(void *)&df_file && df_file.alive);return &df_file;
}
static FILE *df_fopen(const char *path,const char *mode) {
    assert(!df_file.alive && !strcmp(mode,"re"));
    unsigned kind;
    if(!strcmp(path,"/sys/kernel/notes"))kind=1;
    else if(!strcmp(path,"/proc/kallsyms"))kind=2;
    else if(!strcmp(path,AP_COPY_KERNEL_IMAGE))kind=3+df_image_opens++;
    else df_unexpected(path);
    assert(kind<=4);
    df_file=(struct df_file){.alive=true,.kind=kind};df_file_opens++;
    return (FILE *)(void *)&df_file;
}
static size_t df_fread(void *out,size_t size,size_t count,FILE *f) {
    struct df_file *v=df_file_checked(f);assert(size==1);
    const unsigned char *bytes;size_t length;
    if(v->kind==1) {bytes=ap_copy_image_notes;length=sizeof(ap_copy_image_notes);}
    else {
        assert(v->kind>=3 && v->slice);
        bytes=v->slice->bytes;length=v->slice->size;
    }
    assert(v->cursor<=length);
    size_t n=length-v->cursor;if(n>count)n=count;
    memcpy(out,bytes+v->cursor,n);
    if(n && ((df_fault==DF_BUILD_ID && v->kind==1) ||
        (df_fault==DF_IMAGE && v->kind==3 && v->slice->offset==0) ||
        (df_fault==DF_GROUP_IMAGE && v->kind==4)))((unsigned char *)out)[n-1]^=1;
    v->cursor+=n;return n;
}
static int df_fseek(FILE *f,long offset,int whence) {
    struct df_file *v=df_file_checked(f);assert(v->kind>=3 && whence==SEEK_SET);
    const struct ap_copy_image_slice *s=v->kind==3?ap_copy_image_slices:ap_group_image_slices;
    size_t count=v->kind==3?DF_COUNT(ap_copy_image_slices):DF_COUNT(ap_group_image_slices);
    for(size_t i=0;i<count;i++)if(s[i].offset==offset) {v->slice=&s[i];v->cursor=0;return 0;}
    df_unexpected("unmodeled image seek");
}
static int df_ferror(FILE *f) {df_file_checked(f);return 0;}
static int df_fgetc(FILE *f) {
    struct df_file *v=df_file_checked(f);
    assert(v->kind==1 && v->cursor==sizeof(ap_copy_image_notes));return EOF;
}
static char *df_fgets(char *out,int size,FILE *f) {
    struct df_file *v=df_file_checked(f);assert(v->kind==2 && size>1);
    if(v->cursor==sizeof(df_kallsyms)-1)return NULL;
    unsigned n=0;
    do {assert(n+1<(unsigned)size);out[n++]=df_kallsyms[v->cursor++];} while(out[n-1]!='\n');
    out[n]=0;return out;
}
static int df_fclose(FILE *f) {
    df_file_checked(f)->alive=false;df_file_closes++;return 0;
}
static long df_sysconf(int name) {
    assert(name==_SC_PAGESIZE);df_page_queries++;return df_fault==DF_PAGE_SIZE?8192:4096;
}
struct btf *btf__load_vmlinux_btf(void) {
    assert(!df_btf.alive);df_btf.alive=true;df_btf_queries++;return &df_btf;
}
int btf__find_by_name_kind(const struct btf *b,const char *name,__u32 kind) {
    assert(b==&df_btf && b->alive && !strcmp(name,"page") && kind==BTF_KIND_STRUCT);return 1;
}
const struct btf_type *btf__type_by_id(const struct btf *b,__u32 id) {
    static struct btf_type page;
    assert(b==&df_btf && b->alive && id==1);
    page=(struct btf_type){.info=BTF_KIND_STRUCT<<24,.size=df_fault==DF_PAGE_STRUCT?65:64};
    return &page;
}
void btf__free(struct btf *b) {assert(b==&df_btf && b->alive);b->alive=false;}

static void *df_calloc(size_t n,size_t size) {
    assert(n==1 && size==sizeof(df_session) && !df_session_alive);
    memset(&df_session,0,sizeof(df_session));df_session_alive=true;return &df_session;
}
static void df_free(void *p) {
    if(!p)return;
    assert(p==&df_session && df_session_alive);df_session_alive=false;
}
static void *df_realloc(void *p,size_t n) {(void)p;(void)n;df_unexpected("realloc");}
static int df_getsockopt(int fd,int level,int option,void *out,socklen_t *n) {
    (void)fd;(void)level;(void)option;(void)out;(void)n;df_unexpected("getsockopt");
}
static pid_t df_getpid(void) {return DF_PID;}
static long df_syscall(long nr,...) {
    va_list args;va_start(args,nr);
    if(nr==SYS_bpf) {
        int command=va_arg(args,int);
        assert(command==BPF_MAP_GET_FD_BY_ID || command==BPF_PROG_GET_FD_BY_ID || command==BPF_LINK_GET_FD_BY_ID);
        dm_global_id_queries++;va_end(args);errno=EPERM;return -1;
    }
    if(nr==SYS_gettid) {va_end(args);return DF_PID;}
    if(nr==SYS_pidfd_open) {
        assert(va_arg(args,int)==DF_PID && va_arg(args,int)==0 && !df_owner_alive);
        df_owner_alive=true;va_end(args);return DF_PIDFD;
    }
    if(nr==SYS_connect) {
        assert(va_arg(args,int)==-1 && va_arg(args,void *)==NULL && va_arg(args,int)==0);
        assert(df_owner_alive && df_config.anchor_phase==AP_GROUPED_ANCHOR_ARMED);
        /* This is a synthetic map publication, not an actual BPF callback. */
        df_config.anchor_start=17;df_config.anchor_ip=DF_ANCHOR;
        df_config.vmemmap_base=UINT64_C(0xffffea0000000000);
        df_config.page_offset_base=UINT64_C(0xffff888000000000);
        df_config.anchor_phase=df_fault==DF_ANCHOR_FAIL?AP_GROUPED_ANCHOR_ARMED:AP_GROUPED_ANCHORED;
        df_anchor_calls++;va_end(args);errno=EBADF;return -1;
    }
    va_end(args);df_unexpected("syscall (including BPF/perf_event_open)");
}
static int df_poll(struct pollfd *p,nfds_t n,int timeout) {
    assert(n==1 && timeout==0 && p->fd==DF_PIDFD && p->events==POLLIN && !p->revents && df_owner_alive);
    return 0;
}
static int df_close(int fd) {
    assert(fd==DF_PIDFD && df_owner_alive);df_owner_alive=false;df_owner_closes++;return 0;
}

/* Every libbpf import is implemented here, including unreachable operations.
 * There is no weak symbol, dlsym, passthrough or native fallback. */
long libbpf_get_error(const void *p) {
    intptr_t n=(intptr_t)p;return n<0 && n>=-4095?(long)n:0;
}
struct bpf_object *bpf_object__open_file(const char *path,const void *opts) {
    assert(!strcmp(path,"fixture-object-only") && !opts && !df_object.open);
    if(df_fault==DF_OPEN_FAIL)return (struct bpf_object *)(intptr_t)-EACCES;
    df_object.open=true;return &df_object;
}
int bpf_object__load(struct bpf_object *o) {
    assert(o==&df_object && o->open && !o->loaded);
    if(df_fault==DF_LOAD_FAIL) {errno=EIO;return -1;}
    o->loaded=true;return 0;
}
void bpf_object__close(struct bpf_object *o) {
    assert(o==&df_object && o->open);
    assert(df_terminal && df_destroyed==df_attached && !df_ring.alive);
    for(unsigned i=0;i<o->programs;i++)assert(df_programs[i].fd<0);
    for(unsigned i=0;i<o->programs;i++)if(df_programs[i].fd>=0) {
        df_programs[i].fd=-1;df_program_closes++;
    }
    for(unsigned i=0;i<o->maps;i++) {assert(df_maps[i].fd>=0);df_maps[i].fd=-1;df_map_closes++;}
    o->open=false;df_object_closes++;
}
struct bpf_program *bpf_object__next_program(const struct bpf_object *o,struct bpf_program *p) {
    assert(o==&df_object && o->open);
    unsigned at=p?p->at+1:0;
    assert(!p || (p>=df_programs && p<df_programs+o->programs));
    return at<o->programs?&df_programs[at]:NULL;
}
struct bpf_map *bpf_object__next_map(const struct bpf_object *o,const struct bpf_map *p) {
    assert(o==&df_object && o->open);
    unsigned at=p?p->at+1:0;
    assert(!p || (p>=df_maps && p<df_maps+o->maps));
    return at<o->maps?&df_maps[at]:NULL;
}
const char *bpf_program__name(const struct bpf_program *p) {return p->name;}
int bpf_program__fd(const struct bpf_program *p) {return p->fd;}
int bpf_map__fd(const struct bpf_map *p) {return p->fd;}
int bpf_link__fd(const struct bpf_link *p) {assert(p->alive);return p->fd;}
void bpf_program__unload(struct bpf_program *p) {
    if(df_terminal) {
        /* Only the actual ap_close caller enters this modeled terminal phase.
         * No link, ring or map may have been released yet. An unlinked program
         * is legitimate only here, after failed load/partial attachment. */
        assert(p->fd>=0 && !p->unloaded && df_object.open);
        assert(!df_destroyed && !df_ring_closes && !df_object_closes);
        for(unsigned i=0;i<df_attached;i++)assert(df_links[i].alive);
        for(unsigned i=0;i<df_object.maps;i++)assert(df_maps[i].fd>=0);
        df_terminal_unloads++;
    } else {
        assert(p->fd>=0 && !p->unloaded && df_links[p->at].alive);
        assert(df_session.link_identity[p->at].program_id==p->id);
    }
    p->fd=-1;p->unloaded=true;df_unloads++;
}
int bpf_object__find_map_fd_by_name(const struct bpf_object *o,const char *name) {
    assert(o==&df_object && o->loaded);
    if(df_fault==DF_FAULT_MAP_MISSING && !strcmp(name,"stream_copy_faults")) {errno=ENOENT;return -1;}
    for(unsigned i=0;i<o->maps;i++)if(!strcmp(name,df_maps[i].name))return df_maps[i].fd;
    errno=ENOENT;return -1;
}
static struct bpf_link *df_attach(const struct bpf_program *p,unsigned type) {
    assert(df_object.loaded && p->fd>=0 && p->at==df_attached);
    df_attach_calls++;
    if(df_fault==DF_ATTACH_FAIL && p->at==df_bad_at) {errno=EIO;return (struct bpf_link *)(intptr_t)-EIO;}
    struct bpf_link *l=&df_links[df_attached++];
    *l=(struct bpf_link){.at=p->at,.id=201+p->at,.program_id=p->id,.type=type,.fd=3000+(int)p->at,.alive=true};
    if(df_fault==DF_DUP_LINK && p->at==df_bad_at)l->id--;
    if(df_fault==DF_PERF_LINK && p->at==df_bad_at)l->type=BPF_LINK_TYPE_PERF_EVENT;
    return l;
}
struct bpf_link *bpf_program__attach(const struct bpf_program *p) {
    assert(p->type==BPF_PROG_TYPE_TRACING);
    return df_attach(p,p->at<AP_PROGRAMS && df_inventory[p->at].raw?
        BPF_LINK_TYPE_RAW_TRACEPOINT:BPF_LINK_TYPE_TRACING);
}
struct bpf_link *bpf_program__attach_kprobe_multi_opts(const struct bpf_program *p,
        const char *pattern,const struct ap_kprobe_multi_opts *opts) {
    assert(p->type==BPF_PROG_TYPE_KPROBE && !pattern && opts && opts->sz==sizeof(*opts));
    assert(opts->syms && opts->cookies && !opts->addrs && !opts->unique_match);
    unsigned expected;bool session=false,ret=false;
    if(!strcmp(p->name,"fd_so")) {expected=4;session=true;}
    else if(!strcmp(p->name,"fd_si")) {expected=5;session=true;}
    else if(!strcmp(p->name,"fd_s20e"))expected=4;
    else if(!strcmp(p->name,"fd_s20x")) {expected=4;ret=true;}
    else if(!strcmp(p->name,"fd_file_retired") || !strcmp(p->name,"fd_exec_closed_file"))expected=1;
    else if(!strcmp(p->name,"fd_stream_copy_protocol_enter"))expected=20;
    else if(!strcmp(p->name,"fd_stream_copy_protocol_exit")) {expected=4;ret=true;}
    else df_unexpected(p->name);
    assert(opts->cnt==expected && opts->session==session && opts->retprobe==ret);
    struct bpf_link *l=df_attach(p,BPF_LINK_TYPE_KPROBE_MULTI);
    if(libbpf_get_error(l))return l;
    l->count=(unsigned)opts->cnt;l->flags=ret?BPF_F_KPROBE_MULTI_RETURN:0;
    for(unsigned i=0;i<l->count;i++) {
        l->addresses[i]=df_address(opts->syms[i]);l->cookies[i]=opts->cookies[i];
    }
    return l;
}
struct bpf_link *bpf_program__attach_kprobe_opts(const struct bpf_program *p,const char *s,
        const struct ap_kprobe_opts *o) {
    (void)p;(void)s;(void)o;df_unexpected("classic kprobe/perf/tracefs attach");
}
int bpf_link__destroy(struct bpf_link *l) {
    assert(l->alive && df_destroyed<df_attached);
    assert(df_terminal);
    for(unsigned i=0;i<df_object.programs;i++)assert(df_programs[i].fd<0);
    assert(df_object.open && !df_ring_closes && !df_object_closes);
    for(unsigned i=0;i<df_object.maps;i++)assert(df_maps[i].fd>=0);
    df_destroy_order[df_destroyed++]=l->at;l->alive=false;l->fd=-1;
    if(df_destroy_failure==(int)l->at) {errno=EUCLEAN;return -1;}
    return 0;
}
int bpf_map_update_elem(int fd,const void *key,const void *value,unsigned long long flags) {
    assert(fd==1000 && *(const u32 *)key==0 && flags==BPF_ANY);
    const struct ap_config *c=value;assert(c->provider==DF_PROVIDER);
    df_updates++;
    if(df_fault==DF_CONFIG_FAIL) {errno=EIO;return -1;}
    if(!df_updates || df_updates>3)df_unexpected("config update count");
    if(df_updates==1)assert(c->anchor_phase==0 && c->anchor_task==0);
    if(df_updates==2)assert(c->anchor_phase==AP_GROUPED_ANCHOR_ARMED &&
        c->anchor_task==((u64)DF_PID<<32|DF_PID) && df_owner_alive);
    if(df_updates==3)assert(c->anchor_phase==AP_GROUPED_ANCHOR_ACTIVE && c->anchor_ip==DF_ANCHOR);
    df_config=*c;return 0;
}
int bpf_map_lookup_elem(int fd,const void *key,void *out) {
    if(fd==1023) {
        assert(*(const u32 *)key==1);
        memcpy(out,&dm_fault,sizeof(dm_fault));dm_fault_reads++;
        if(dm_unstable && dm_fault_reads%2==0)((struct ap_stream_fault_state *)out)->attempt++;
        return 0;
    }
    assert(fd==1000 && *(const u32 *)key==0 && df_owner_alive);
    df_lookups++;memcpy(out,&df_config,sizeof(df_config));return 0;
}
int bpf_map_delete_elem(int fd,const void *key) {(void)fd;(void)key;df_unexpected("map delete");}
struct ring_buffer *ring_buffer__new(int fd,int (*cb)(void *,void *,size_t),void *ctx,const void *opts) {
    assert(fd==1022 && cb==stream_copy_record && ctx==&df_session && !opts && !df_ring.alive);
    if(df_fault==DF_RING_FAIL) {errno=ENOMEM;return NULL;}
    df_ring.alive=true;return &df_ring;
}
void ring_buffer__free(struct ring_buffer *r) {assert(r==&df_ring && r->alive);r->alive=false;df_ring_closes++;}
struct ring *ring_buffer__ring(struct ring_buffer *r,unsigned n) {(void)r;(void)n;df_unexpected("ring query");}
unsigned long ring__consumer_pos(const struct ring *r) {(void)r;df_unexpected("ring consumer");}
unsigned long ring__producer_pos(const struct ring *r) {(void)r;df_unexpected("ring producer");}
int ring_buffer__consume(struct ring_buffer *r) {(void)r;df_unexpected("ring consume");}
int ring_buffer__epoll_fd(const struct ring_buffer *r) {(void)r;df_unexpected("ring epoll");}

int bpf_obj_get_info_by_fd(int fd,void *out,unsigned int *size) {
    df_info_queries++;
    if(dm_info_error){errno=EIO;return -1;}
    for(unsigned at=0;at<df_object.maps;at++)if(df_maps[at].fd==fd) {
        assert(*size==sizeof(struct bpf_map_info));
        struct bpf_map_info *m=out;m->id=df_maps[at].id;m->type=df_maps[at].type;
        memcpy(m->name,df_maps[at].name,strlen(df_maps[at].name)<15?strlen(df_maps[at].name):15);
        if(dm_info_short)*size=0;
        if(!strcmp(df_maps[at].name,"stream_copy_faults")) {
            m->key_size=sizeof(u32);m->value_size=sizeof(struct ap_stream_fault_state);m->max_entries=AP_COMMANDS;
            if(df_fault==DF_FAULT_MAP)m->value_size--;
        }
        return 0;
    }
    if(fd>=2000 && fd<2000+(int)df_object.programs) {
        unsigned at=(unsigned)(fd-2000);assert(df_programs[at].fd==fd && *size==sizeof(struct bpf_prog_info));
        struct bpf_prog_info *p=out;p->id=df_programs[at].id;p->type=df_programs[at].type;
        if(at==df_bad_at && df_fault==DF_PROGRAM_MISS)p->recursion_misses=7;
        if(!strcmp(df_programs[at].name,"fd_original_read_entered")) {p->attach_btf_obj_id=11;p->attach_btf_id=19;}
        if(!strcmp(df_programs[at].name,"fd_stream_fault_enter") || !strcmp(df_programs[at].name,"fd_stream_fault_exit")) {
            p->attach_btf_obj_id=11;p->attach_btf_id=108812;
            if(at==df_bad_at && df_fault==DF_FAULT_MISS)p->recursion_misses=1;
            if(at==df_bad_at && df_fault==DF_FAULT_SHORT)*size=offsetof(struct bpf_prog_info,recursion_misses);
        }
        return 0;
    }
    if(fd>=3000 && fd<3000+(int)df_attached) {
        struct bpf_link *l=&df_links[fd-3000];assert(l->alive && l->fd==fd && *size==sizeof(struct bpf_link_info));
        struct bpf_link_info *v=out;
        u64 *addresses=(u64 *)(uintptr_t)v->kprobe_multi.addrs;
        u64 *cookies=(u64 *)(uintptr_t)v->kprobe_multi.cookies;
        unsigned capacity=v->kprobe_multi.count;
        l->queries++;
        if(l->type==BPF_LINK_TYPE_KPROBE_MULTI) {
            assert((addresses!=NULL)==(capacity!=0));
            if(capacity) {
                assert(cookies && capacity>=l->count && capacity<=AP_MEMBERSHIP_ENTRY_COUNT);
                for(unsigned i=0;i<l->count;i++) {
                    /* Kernel order is not attachment order: real validator
                     * must bind address/cookie pairs, not just compare arrays. */
                    unsigned from=l->count-1-i;
                    addresses[i]=l->addresses[from];cookies[i]=l->cookies[from];
                }
                l->shape_queries++;
                if(l->at==df_bad_at) {
                    if(df_fault==DF_COOKIE)cookies[0]^=UINT64_C(0x100);
                    if(df_fault==DF_ADDRESS)addresses[0]++;
                    if(df_fault==DF_SHORT_INFO)*size=offsetof(struct bpf_link_info,kprobe_multi.cookies);
                }
            }
            v->kprobe_multi.count=l->count;v->kprobe_multi.flags=l->flags;
            if(l->at==df_bad_at && df_fault==DF_LINK_MISS)v->kprobe_multi.missed=11;
            if(capacity && l->at==df_bad_at && df_fault==DF_COUNT_BAD)v->kprobe_multi.count--;
            if(capacity && l->at==df_bad_at && df_fault==DF_FLAGS)v->kprobe_multi.flags^=BPF_F_KPROBE_MULTI_RETURN;
        }
        if(l->at==df_index("fd_original_read_entered")) {
            v->tracing.attach_type=BPF_TRACE_FENTRY;v->tracing.target_obj_id=11;v->tracing.target_btf_id=19;
            if(df_fault==DF_READ_LINK)v->tracing.cookie=1;
        }
        if(!strcmp(df_programs[l->at].name,"fd_stream_fault_enter") ||
           !strcmp(df_programs[l->at].name,"fd_stream_fault_exit")) {
            v->tracing.attach_type=!strcmp(df_programs[l->at].name,"fd_stream_fault_enter")?
                BPF_TRACE_FENTRY:BPF_TRACE_FEXIT;
            v->tracing.target_obj_id=11;v->tracing.target_btf_id=108812;
            if(l->at==df_bad_at && df_fault==DF_FAULT_TARGET)v->tracing.target_btf_id++;
        }
        v->type=l->type;v->id=l->id;v->prog_id=l->program_id;return 0;
    }
    df_unexpected("info for unknown or released fd");
}

static void df_reset(enum df_fault fault,unsigned at) {
    assert(!df_session_alive && !df_object.open && !df_ring.alive && !df_btf.alive && !df_file.alive && !df_owner_alive);
    assert(!df_terminal);df_terminal_unloads=0;
    df_fault=fault;df_bad_at=at;df_destroy_failure=-1;
    df_attached=df_attach_calls=df_unloads=df_destroyed=df_map_closes=0;
    df_program_closes=df_object_closes=df_ring_closes=df_updates=df_lookups=0;
    df_file_opens=df_file_closes=df_image_opens=df_page_queries=df_btf_queries=0;
    df_anchor_calls=df_owner_closes=df_info_queries=0;
    memset(df_links,0,sizeof(df_links));memset(&df_config,0,sizeof(df_config));
    df_object=(struct bpf_object){.programs=AP_PROGRAMS,.maps=DF_MAPS};
    if(fault==DF_MISSING_PROGRAM)df_object.programs--;
    if(fault==DF_EXTRA_PROGRAM)df_object.programs++;
    if(fault==DF_MISSING_MAP)df_object.maps--;
    if(fault==DF_EXTRA_MAP)df_object.maps++;
    for(unsigned i=0;i<df_object.programs;i++) {
        df_programs[i]=(struct bpf_program){.at=i,.id=101+i,.type=BPF_PROG_TYPE_TRACING,
            .fd=2000+(int)i,.name="extra-fixture-program"};
        if(i<AP_PROGRAMS) {df_programs[i].name=df_inventory[i].name;df_programs[i].type=df_inventory[i].type;}
    }
    /* Remove only generic held_fd_probe: retain every independently required
     * session, Read, retirement and membership program. This makes the exact
     * cardinality mutant sensitive, rather than failing an earlier fixture
     * prerequisite because the final membership-return program was absent. */
    if(fault==DF_MISSING_PROGRAM) {
        df_programs[0].name=df_inventory[AP_PROGRAMS-1].name;
        df_programs[0].type=df_inventory[AP_PROGRAMS-1].type;
    }
    for(unsigned i=0;i<df_object.maps;i++)df_maps[i]=(struct bpf_map){
        .at=i,.id=301+i,.fd=1000+(int)i,.name=i<DF_MAPS?df_map_names[i]:"extra-fixture-map"};
    /* Remove an unconsulted map, preserving the real config/ring prerequisites. */
    if(fault==DF_MISSING_MAP) {
        for(unsigned i=2;i<DF_MAPS-1;i++)df_maps[i].name=df_map_names[i+1];
        df_maps[DF_MAPS-3].fd=1022; /* retained ring keeps its independently fixed FD */
        df_maps[DF_MAPS-2].fd=1023; /* new witness map also remains present */
    }
    if(fault==DF_MISSING_REQUIRED_MAP)df_maps[0].name="missing-config";
    for(unsigned i=0;i<df_object.maps;i++)df_maps[i].type=df_map_type(df_maps[i].name);
    if(fault==DF_DUP_MAP)df_maps[2].id=df_maps[1].id;
    if(fault==DF_DUP_PROGRAM) {
        assert(at>0);df_programs[at].id=df_programs[at-1].id;
    }
    if(fault==DF_WRONG_PROGRAM_KIND)df_programs[at].type=BPF_PROG_TYPE_KPROBE;
    if(fault==DF_LEGACY_PROGRAM)df_programs[at].name="fd_stream_copy_enter";
    errno=0;
}
static int df_terminal_close(struct ap_session *s) {
    assert(!df_terminal);df_terminal=true;
    int result=ap_close(s);
    df_terminal=false;return result;
}
static void df_closed(struct ap_session *s,int expected_close) {
    unsigned attached=df_attached,programs=df_object.programs,maps=df_object.maps;
    bool object=df_object.open,ring=df_ring.alive;
    assert(df_terminal_close(s)==expected_close);
    assert(df_destroyed==attached && df_ring_closes==(unsigned)ring && df_object_closes==(unsigned)object);
    if(object)assert(df_program_closes+df_unloads==programs && df_map_closes==maps);
    for(unsigned i=0;i<attached;i++)assert(df_destroy_order[i]==attached-1-i && !df_links[i].alive);
    assert(!df_session_alive && !df_object.open && !df_ring.alive && !df_owner_alive);
    assert(!df_file.alive && df_file_opens==df_file_closes && !df_btf.alive);
}
static void df_inventory_exact(struct ap_session *s) {
    struct ap_program_id ids[DF_TOTAL];u32 written=0;
    assert(ap_identifiers(s,ids,DF_TOTAL,&written)==0 && written==DF_TOTAL);
    unsigned counts[3]={0};
    for(unsigned i=0;i<written;i++) {
        assert(ids[i].kind<3 && ids[i].id);
        for(unsigned j=0;j<i;j++)assert(ids[i].kind!=ids[j].kind || ids[i].id!=ids[j].id);
        counts[ids[i].kind]++;
    }
    assert(counts[0]==DF_MAPS && counts[1]==AP_PROGRAMS && counts[2]==AP_LINKS);
}
static void df_positive(void) {
    df_reset(DF_OK,0);struct ap_session *s=NULL;
    assert(ap_open("fixture-object-only",DF_PROVIDER,&s)==0 && s==&df_session && s->ready);
    assert(ap_provider_topology_version()==2 && df_attached==49 && df_attach_calls==49);
    assert(df_page_queries==1 && df_btf_queries==1 && df_image_opens==2);
    assert(df_anchor_calls==1 && df_owner_closes==1 && df_updates==3 && df_lookups==2);
    assert(df_unloads==38); /* 41 tracing programs minus retained Read entry and two fault hooks. */
    for(unsigned i=0;i<49;i++) {
        assert(s->links[i]==&df_links[i] && s->link_identity[i].id==201+i &&
            s->link_identity[i].program_id==101+i);
        if(df_inventory[i].type==BPF_PROG_TYPE_KPROBE)assert(df_links[i].shape_queries>0);
    }
    df_inventory_exact(s);df_closed(s,0);
}
static void df_refusal(enum df_fault fault,unsigned at,int expected_errno) {
    df_reset(fault,at);struct ap_session *s=NULL;
    int primary=ap_open("fixture-object-only",DF_PROVIDER,&s),saved=errno;
    assert(primary==-1);
    if(expected_errno)assert(saved==expected_errno);
    assert(!s || !s->ready);
    if(fault>=DF_PAGE_SIZE && fault<=DF_GROUP_IMAGE)assert(!s && !df_object.open && !df_attach_calls);
    if(fault==DF_PERF_LINK || fault==DF_DUP_LINK || fault==DF_DUP_PROGRAM)
        assert(s && df_attached==at+1 && !df_anchor_calls);
    if(fault==DF_COOKIE || fault==DF_ADDRESS || fault==DF_COUNT_BAD || fault==DF_FLAGS || fault==DF_SHORT_INFO)
        assert(s && df_attached==49 && df_anchor_calls==1 && df_links[at].shape_queries>0);
    df_closed(s,0);
    /* Capture before cleanup, as the real FFI does; errno after successful
     * cleanup alone is not the primary. No success is substituted here. */
    assert(primary==-1 && (!expected_errno || saved==expected_errno));
}
static unsigned df_partial_prefixes;
static void df_partial(void) {
    assert(!df_partial_prefixes);
    for(unsigned at=0;at<AP_PROGRAMS;at++) {
        df_reset(DF_ATTACH_FAIL,at);struct ap_session *s=NULL;
        int primary=ap_open("fixture-object-only",DF_PROVIDER,&s),saved=errno;
        assert(primary==-1 && saved==EIO && s && !s->ready && df_attached==at && df_attach_calls==at+1);
        struct ap_program_id ids[DF_TOTAL];u32 written=0;
        assert(ap_identifiers(s,ids,DF_TOTAL,&written)==0 && written==DF_MAPS+AP_PROGRAMS+at);
        df_destroy_failure=at?((int)at-1):-1;
        df_closed(s,at?-1:0);
        assert(primary==-1 && saved==EIO); /* EUCLEAN is secondary, never success. */
        df_partial_prefixes++;
    }
    assert(df_partial_prefixes==49);
}
static void df_map_census(enum df_fault fault) {
    df_reset(fault,0);struct ap_session *s=NULL;
    /* Missing/duplicate non-required map is not an ap_open refusal. This
     * intentionally exposes the actual boundary: ProviderReady::validate in
     * Rust must reject the resulting incomplete inventory. Do not count this
     * as execution of that Rust validator. Extra maps exceed its original
     * exact-capacity buffer in the real C identifier walk. */
    assert(ap_open("fixture-object-only",DF_PROVIDER,&s)==0);
    struct ap_program_id ids[DF_TOTAL];u32 n=0;
    int rc=ap_identifiers(s,ids,DF_TOTAL,&n),saved=errno;
    if(fault==DF_EXTRA_MAP)assert(rc==-1 && saved==EOVERFLOW && n==DF_TOTAL);
    else {
        assert(rc==0 && n==DF_TOTAL-1);
        unsigned maps=0;
        for(unsigned i=0;i<n;i++)maps+=ids[i].kind==0;
        assert(maps==23);
    }
    df_closed(s,0);
}
static void df_reused_program(void) {
    df_reset(DF_OK,0);struct ap_session *s=NULL;
    assert(ap_open("fixture-object-only",DF_PROVIDER,&s)==0);
    unsigned from=df_index("fd_so"),to=df_index("fd_si");
    /* Session programs legitimately bind multiple distinct sites inside each
     * one link. FtraceV1 declares no shared links; a second original link for
     * the same loaded program must refuse, unlike classic's explicit reuse. */
    struct ap_link_identity before=s->link_identity[to];
    df_links[to].program_id=df_programs[from].id;
    assert(bind_observer_link(s,to,&df_programs[from],false)==-1);
    assert(!memcmp(&before,&s->link_identity[to],sizeof(before)));
    df_links[to].program_id=before.program_id;
    df_inventory_exact(s);df_closed(s,0);
}
static void df_link_census(void) {
    df_reset(DF_OK,0);struct ap_session *s=NULL;
    assert(ap_open("fixture-object-only",DF_PROVIDER,&s)==0);
    s->ready=false;
    /* Mutating owned memory is an explicit census premise, not a fabricated
     * kernel receipt. The same production finalizer must refuse each shape. */
    s->links_count=AP_LINKS-1;assert(finish_observer_load(s)==-1);
    s->links_count=AP_LINKS+1;assert(finish_observer_load(s)==-1);
    s->links_count=AP_LINKS;
    assert(finish_observer_load(s)==0);
    df_inventory_exact(s);df_closed(s,0);
}

/* Transport only: emit exactly the rows returned by the production identifier
 * walk. These libbpf boundaries are modeled premises, not kernel receipts.
 * Rust must apply the original ProviderReady validator to these same rows. */
static void df_export_inventory(void) {
    static const struct { const char *name; enum df_fault fault; } cases[]={
        {"good",DF_OK},{"missing-map",DF_MISSING_MAP},
        {"duplicate-map",DF_DUP_MAP},{"extra-map",DF_EXTRA_MAP}
    };
    printf("{\"schema\":1,\"fixture_only\":true,\"provider_incarnation\":%llu,\"cases\":[",
        (unsigned long long)DF_PROVIDER);
    for(unsigned c=0;c<DF_COUNT(cases);c++) {
        df_reset(cases[c].fault,0);struct ap_session *s=NULL;
        errno=0;
        int opened=ap_open("fixture-object-only",DF_PROVIDER,&s),open_errno=errno;
        assert(opened==0 && s && s->ready);
        struct ap_program_id ids[DF_TOTAL];u32 written=0;
        errno=0;
        int inventory=ap_identifiers(s,ids,DF_TOTAL,&written),inventory_errno=errno;
        assert(written<=DF_TOTAL);
        if(cases[c].fault==DF_EXTRA_MAP)
            assert(inventory==-1 && inventory_errno==EOVERFLOW && written==DF_TOTAL);
        else assert(inventory==0 && written==(cases[c].fault==DF_OK?DF_TOTAL:DF_TOTAL-1));
        /* No later successful close can replace either saved original result. */
        df_closed(s,0);
        printf("%s{\"name\":\"%s\",\"open_result\":%d,\"open_errno\":%d,"
            "\"inventory_result\":%d,\"inventory_errno\":%d,\"capacity\":%u,"
            "\"written\":%u,\"ids\":[",c?",":"",cases[c].name,opened,open_errno,
            inventory,inventory_errno,(unsigned)DF_TOTAL,written);
        for(unsigned i=0;i<written;i++)
            printf("%s{\"kind\":%u,\"id\":%u}",i?",":"",ids[i].kind,ids[i].id);
        printf("]}");
    }
    printf("]}\n");
}

/* Exercise actual Sendto preparation before submit, using the existing full
 * driver/load model. Each fresh-query refusal retains the original errno and
 * leaves every pending slot, command counter and kernel-shaped map untouched.
 * These host premises identify the reachable native diagnostic branches; they
 * do not identify which one failed in a particular kernel run. */
static void df_sendto_readiness(void) {
    static const struct {const char *program;enum df_fault fault;} cases[]={
        {"fd_original_read_entered",DF_PROGRAM_MISS},
        {"fd_stream_fault_enter",DF_PROGRAM_MISS},
        {"fd_stream_fault_exit",DF_PROGRAM_MISS},
        {"fd_stream_copy_protocol_enter",DF_PROGRAM_MISS},
        {"fd_stream_copy_protocol_exit",DF_LINK_MISS},
        {"fd_so",DF_PROGRAM_MISS},
        {"fd_si",DF_LINK_MISS},
        {"fd_s20e",DF_PROGRAM_MISS},
        {"fd_s20x",DF_LINK_MISS},
        {"fd_s20e",DF_ADDRESS},
        {"fd_s20x",DF_SHORT_INFO}
    };
    const struct ap_pending_command empty={0};
    for(unsigned i=0;i<DF_COUNT(cases);i++) {
        df_reset(DF_OK,0);struct ap_session *s=NULL;
        assert(!ap_open("fixture-object-only",DF_PROVIDER,&s) && s->ready);
        assert(!stream_copy_observer_ready(s) && !fd_accept_observer_ready(s));
        const u64 next=s->next_command;const unsigned updates=df_updates;
        df_bad_at=df_index(cases[i].program);df_fault=cases[i].fault;
        u64 command=UINT64_C(0xfeedface);errno=0;
        assert(ap_prepare_original_sendto(s,DF_PIDFD,395,1,5,0x5555555b1ca0,79,0x4000,&command)==-1);
        assert(errno==ENODATA && command==UINT64_C(0xfeedface));
        assert(s->next_command==next && df_updates==updates);
        for(unsigned slot=0;slot<AP_COMMANDS;slot++)
            assert(!memcmp(&s->pending[slot],&empty,sizeof(empty)));
        /* Distinguish the preexisting policies without changing either one.
         * Shared global counters are not task-scoped completion evidence. */
        if(i==7 || i==8) {
            assert(fd_accept_observer_ready(s)==-1 && errno==ENODATA);
            assert(!fd_accept_observer_ready_runtime(s));
        }
        if(i>=9)assert(fd_accept_observer_ready_runtime(s)==-1 && errno==ENODATA);
        df_fault=DF_OK;
        assert(!stream_copy_observer_ready(s) && !fd_accept_observer_ready(s));
        /* Preparation released its serialization flag even on refusal. */
        assert(!enter_commands(s));leave_commands(s);
        df_closed(s,0);
    }
    df_reset(DF_OK,0);struct ap_session *s=NULL;
    assert(!ap_open("fixture-object-only",DF_PROVIDER,&s) && s->ready);
    dm_info_error=true;u64 command=UINT64_C(0xfeedface);errno=0;
    assert(ap_prepare_original_sendto(s,DF_PIDFD,395,1,5,0x5555555b1ca0,79,0x4000,&command)==-1);
    assert(errno==EIO && command==UINT64_C(0xfeedface));
    dm_info_error=false;assert(!enter_commands(s));leave_commands(s);df_closed(s,0);
}

static const char *const df_selectors[]={
    "positive","page-size","page-struct","build-id","image","group-image",
    "missing-program","extra-program","duplicate-program","duplicate-link","perf-link",
    "session-cookie","session-address","session-count","session-flags","short-link-info",
    "shared-cookie","shared-flags","membership-address","membership-count","retirement-cookie",
    "read-link","anchor","wrong-program-kind","legacy-program","required-map",
    "missing-map-census","extra-map-census","duplicate-map-census","partial-attach",
    "load-failure","config-failure","ring-failure","program-reuse","link-census",
    "fault-entry-target","fault-exit-target","fault-entry-miss","fault-exit-miss",
    "fault-entry-short","fault-exit-short","fault-map-shape","fault-map-missing",
    "fault-runtime-miss","sendto-readiness"
};
static void df_case(unsigned n) {
    switch(n) {
    case 0:df_positive();break;
    case 1:df_refusal(DF_PAGE_SIZE,0,ESTALE);break;
    case 2:df_refusal(DF_PAGE_STRUCT,0,ESTALE);break;
    case 3:df_refusal(DF_BUILD_ID,0,ESTALE);break;
    case 4:df_refusal(DF_IMAGE,0,ESTALE);break;
    case 5:df_refusal(DF_GROUP_IMAGE,0,ESTALE);break;
    case 6:df_refusal(DF_MISSING_PROGRAM,0,0);break;
    case 7:df_refusal(DF_EXTRA_PROGRAM,0,EOVERFLOW);break;
    case 8:df_refusal(DF_DUP_PROGRAM,1,ENODATA);break;
    case 9:df_refusal(DF_DUP_LINK,1,ENODATA);break;
    case 10:df_refusal(DF_PERF_LINK,0,ENODATA);break;
    case 11:df_refusal(DF_COOKIE,df_index("fd_so"),ENODATA);break;
    case 12:df_refusal(DF_ADDRESS,df_index("fd_so"),ENODATA);break;
    case 13:df_refusal(DF_COUNT_BAD,df_index("fd_so"),ENODATA);break;
    case 14:df_refusal(DF_FLAGS,df_index("fd_so"),ENODATA);break;
    case 15:df_refusal(DF_SHORT_INFO,df_index("fd_so"),ENODATA);break;
    case 16:df_refusal(DF_COOKIE,df_index("fd_s20e"),ENODATA);break;
    case 17:df_refusal(DF_FLAGS,df_index("fd_s20x"),ENODATA);break;
    case 18:df_refusal(DF_ADDRESS,df_index("fd_stream_copy_protocol_enter"),ENODATA);break;
    case 19:df_refusal(DF_COUNT_BAD,df_index("fd_stream_copy_protocol_exit"),ENODATA);break;
    case 20:df_refusal(DF_COOKIE,df_index("fd_file_retired"),ENODATA);break;
    case 21:df_refusal(DF_READ_LINK,0,ENODATA);break;
    case 22:df_refusal(DF_ANCHOR_FAIL,0,ENODATA);break;
    case 23:df_refusal(DF_WRONG_PROGRAM_KIND,0,ENODATA);break;
    case 24:df_refusal(DF_LEGACY_PROGRAM,0,ENODATA);break;
    case 25:df_refusal(DF_MISSING_REQUIRED_MAP,0,ENODATA);break;
    case 26:df_map_census(DF_MISSING_MAP);break;
    case 27:df_map_census(DF_EXTRA_MAP);break;
    case 28:df_map_census(DF_DUP_MAP);break;
    case 29:df_partial();break;
    case 30:df_refusal(DF_LOAD_FAIL,0,EIO);break;
    case 31:df_refusal(DF_CONFIG_FAIL,0,EIO);break;
    case 32:df_refusal(DF_RING_FAIL,0,ENOMEM);break;
    case 33:df_reused_program();break;
    case 34:df_link_census();break;
    case 35:df_refusal(DF_FAULT_TARGET,df_index("fd_stream_fault_enter"),ENODATA);break;
    case 36:df_refusal(DF_FAULT_TARGET,df_index("fd_stream_fault_exit"),ENODATA);break;
    case 37:df_refusal(DF_FAULT_MISS,df_index("fd_stream_fault_enter"),ENODATA);break;
    case 38:df_refusal(DF_FAULT_MISS,df_index("fd_stream_fault_exit"),ENODATA);break;
    case 39:df_refusal(DF_FAULT_SHORT,df_index("fd_stream_fault_enter"),ENODATA);break;
    case 40:df_refusal(DF_FAULT_SHORT,df_index("fd_stream_fault_exit"),ENODATA);break;
    case 41:df_refusal(DF_FAULT_MAP,0,ENODATA);break;
    case 42:df_refusal(DF_FAULT_MAP_MISSING,0,ENODATA);break;
    case 43:
        for(unsigned i=0;i<2;i++) {
            df_reset(DF_OK,0);struct ap_session *s=NULL;
            assert(!ap_open("fixture-object-only",DF_PROVIDER,&s) && s->ready);
            assert(!stream_copy_observer_ready(s));
            df_bad_at=df_index(i?"fd_stream_fault_exit":"fd_stream_fault_enter");df_fault=DF_FAULT_MISS;
            assert(stream_copy_observer_ready(s)==-1 && errno==ENODATA);
            df_closed(s,0);
        }
        break;
    case 44:df_sendto_readiness();break;
    default:df_unexpected("selector index");
    }
    df_cases++;printf("driver-ftrace fixture case passed: %s\n",df_selectors[n]);
}
static int inherited_main(int argc,char **argv) {
    assert(argc<=2);
    if(argc==2 && !strcmp(argv[1],"inventory-export")) {
        df_export_inventory();
        return 0;
    }
    if(argc==1 || !strcmp(argv[1],"all")) {
        for(unsigned i=0;i<DF_COUNT(df_selectors);i++)df_case(i);
        assert(df_cases==45 && df_partial_prefixes==49); /* original 44 plus Sendto readiness */
    } else {
        bool found=false;
        for(unsigned i=0;i<DF_COUNT(df_selectors);i++)if(!strcmp(argv[1],df_selectors[i])) {df_case(i);found=true;break;}
        assert(found && df_cases==1);
    }
    printf("driver-ftrace: %u selector cases; %u partial prefixes; modeled boundaries, no kernel qualification\n",
        df_cases,df_partial_prefixes);
    return 0;
}

/* Additional terminal-order controls. Keep the original 44 selectors and all
 * 49 partial-attachment prefixes above intact. These premises model libbpf FD
 * ownership only; neither an unload nor a destroy proves kernel map absence. */
static void df_terminal_ready(bool destroy_failure) {
    static const char *const retained[]={
        "fd_original_read_entered","fd_so","fd_si","fd_s20e","fd_s20x",
        "fd_file_retired","fd_exec_closed_file","fd_stream_copy_protocol_enter",
        "fd_stream_copy_protocol_exit","fd_stream_fault_enter","fd_stream_fault_exit"
    };
    _Static_assert(DF_COUNT(retained)==11,"active Ftrace metadata readers");
    df_reset(DF_OK,0);struct ap_session *s=NULL;
    assert(!ap_open("fixture-object-only",DF_PROVIDER,&s) && s && s->ready);
    assert(df_unloads==38 && df_terminal_unloads==0 && df_destroyed==0);
    for(unsigned i=0;i<AP_PROGRAMS;i++) {
        bool expected=false;
        for(unsigned j=0;j<DF_COUNT(retained);j++)expected|=!strcmp(df_programs[i].name,retained[j]);
        assert((df_programs[i].fd>=0)==expected);
    }
    df_inventory_exact(s); /* The active readers still use the original FDs. */
    if(destroy_failure)df_destroy_failure=23;
    df_closed(s,destroy_failure?-1:0);
    assert(df_terminal_unloads==11 && df_unloads==49 && df_program_closes==0);
    if(destroy_failure)assert(errno==EUCLEAN);
}
static void df_terminal_unattached(void) {
    df_reset(DF_CONFIG_FAIL,0);struct ap_session *s=NULL;
    int primary=ap_open("fixture-object-only",DF_PROVIDER,&s),saved=errno;
    assert(primary==-1 && saved==EIO && s && !s->ready && df_object.loaded);
    assert(!df_attached && !df_unloads && !df_ring.alive);
    df_closed(s,0);
    assert(df_terminal_unloads==49 && df_program_closes==0);
    assert(primary==-1 && saved==EIO);
}
static void df_terminal_unloaded(void) {
    for(unsigned mixed=0;mixed<2;mixed++) {
        df_reset(DF_LOAD_FAIL,0);struct ap_session *s=NULL;
        int primary=ap_open("fixture-object-only",DF_PROVIDER,&s),saved=errno;
        assert(primary==-1 && saved==EIO && s && !s->ready && !df_object.loaded);
        /* Explicit failed-load premises: first no load FDs, then a mixture of
         * valid and absent FDs. Never manufacture a successful kernel load. */
        unsigned remaining=0;
        for(unsigned i=0;i<df_object.programs;i++) {
            if(!mixed || i%2==0)df_programs[i].fd=-1;
            else remaining++;
        }
        assert(df_terminal_close(s)==0);
        assert(df_terminal_unloads==remaining && df_unloads==remaining && !df_program_closes);
        assert(!df_destroyed && !df_ring_closes && df_map_closes==DF_MAPS && df_object_closes==1);
        assert(!df_session_alive && !df_object.open && !df_ring.alive && !df_owner_alive);
        assert(!df_btf.alive && !df_file.alive && df_file_opens==df_file_closes);
        assert(primary==-1 && saved==EIO);
    }
}
static void df_terminal_null(void) {
    df_reset(DF_OPEN_FAIL,0);struct ap_session *s=NULL;
    assert(ap_open(NULL,DF_PROVIDER,&s)==-1 && errno==EINVAL && !s);
    df_closed(NULL,0);
    int primary=ap_open("fixture-object-only",DF_PROVIDER,&s),saved=errno;
    assert(primary==-1 && saved==EACCES && s && !s->object && !s->ready);
    df_closed(s,0);
    assert(!df_unloads && !df_terminal_unloads && !df_destroyed && !df_object_closes);
    assert(primary==-1 && saved==EACCES);
}
static void df_terminal_cases(const char *selector) {
    static const char *const names[]={"ready","unattached","unloaded","null","destroy-error"};
    unsigned ran=0;
    for(unsigned i=0;i<DF_COUNT(names);i++)if(!strcmp(selector,"all") || !strcmp(selector,names[i])) {
        switch(i) {
        case 0:df_terminal_ready(false);break;
        case 1:df_terminal_unattached();break;
        case 2:df_terminal_unloaded();break;
        case 3:df_terminal_null();break;
        case 4:df_terminal_ready(true);break;
        }
        ran++;printf("driver-ftrace terminal order case passed: %s\n",names[i]);
    }
    assert(ran==(!strcmp(selector,"all")?5U:1U));
}

#include "provider-open-observation.h"
static int refuse(void) {errno=EPROTO;return -1;}
#include "legacy-id-probe.h"
static unsigned dm_checks;
#define CHECK(x) do {assert(x);dm_checks++;}while(0)
static void dm_ready(struct ap_session **s) {
    dm_fault_reads=0;dm_unstable=dm_info_short=dm_info_error=false;
    df_reset(DF_OK,0);assert(!ap_open("fixture-object-only",DF_PROVIDER,s));
}
static void dm_prepare(struct ap_session *s) {
    struct ap_pending_command *p=&s->pending[ap_command_slot(1)];
    p->state=AP_SLOT_COLLECTED;p->original_selected=true;p->original_collected=true;
    p->submitted=(struct ap_task_command){.provider=DF_PROVIDER,.command=1,.operation=AP_ORIGINAL_READ,
        .expected_object=4,.generation_before=4096,.generation_after=1,.expected_level=7,.original_count=512};
    p->original_selection=(struct ap_original_selection){.provider=DF_PROVIDER,.command=1,.call=4,
        .owner_mm=1,.task=100,.task_start=200,.table=300,.file=400,.user_address=4096,
        .ready=1,.requested_fd=7,.original_count=512};
    p->original_receipt.command=1;p->original_receipt.operation=AP_ORIGINAL_READ;
    p->original_receipt.original.selection=p->original_selection;p->original_receipt.selected_file=400;
    p->original_receipt.selection.word=0xffff800000000400ULL;
    dm_fault=(struct ap_stream_fault_state){.provider=DF_PROVIDER,.command=1,.call=4,
        .task=100,.start=200,.file=400,.pointer=0xffff800000000400ULL,.ubuf=4096,.attempt=1};
}
static void dm_inventory(void) {
    struct ap_session *s=NULL;dm_ready(&s);
    struct ap_program_id ids[122];u32 n=0;assert(!ap_identifiers(s,ids,122,&n) && n==122);
    unsigned counts[3]={0},linked=0;
    for(u32 i=0;i<n;i++) {
        struct ap_owned_metadata m={0};
        assert(!ap_owned_object_info(s,ids[i],&m));
        assert(m.version==1 && !memcmp(&m.identity,&ids[i],sizeof(ids[i])));
        counts[ids[i].kind]++;
        if(m.source==AP_OWNED_PROGRAM_LINK) {
            assert(ids[i].kind==1 && m.value.linked_program.link.prog_id==ids[i].id);
            assert(ap_ftrace_program_link_pair_allowed(m.value.linked_program.program_type,
                m.value.linked_program.link.type,BPF_LINK_TYPE_PERF_EVENT));linked++;
        } else assert(m.source==AP_OWNED_DIRECT_FD);
    }
    CHECK(counts[0]==24 && counts[1]==49 && counts[2]==49 && linked==38 && dm_global_id_queries==0);
    df_closed(s,0);
}
static void dm_identity(void) {
    struct ap_session *s=NULL;dm_ready(&s);struct ap_owned_metadata m,prior;
    memset(&m,0xa5,sizeof(m));prior=m;
    CHECK(ap_owned_object_info(s,(struct ap_program_id){0,999},&m)==-1 && !memcmp(&m,&prior,sizeof(m)));
    dm_info_short=true;CHECK(ap_owned_object_info(s,(struct ap_program_id){0,301},&m)==-1);dm_info_short=false;
    dm_info_error=true;CHECK(ap_owned_object_info(s,(struct ap_program_id){0,301},&m)==-1 && errno==EIO);dm_info_error=false;
    u32 saved=df_links[0].program_id;df_links[0].program_id++;
    CHECK(ap_owned_object_info(s,(struct ap_program_id){1,101},&m)==-1);df_links[0].program_id=saved;
    saved=s->link_identity[0].program_type;s->link_identity[0].program_type=0;
    CHECK(ap_owned_object_info(s,(struct ap_program_id){1,101},&m)==-1);s->link_identity[0].program_type=saved;
    atomic_flag_test_and_set(&s->command_busy);
    CHECK(ap_owned_object_info(s,(struct ap_program_id){0,301},&m)==-1 && errno==EBUSY);leave_commands(s);
    df_closed(s,0);
    df_reset(DF_ATTACH_FAIL,5);s=NULL;CHECK(ap_open("fixture-object-only",DF_PROVIDER,&s)==-1 && s && !s->ready);
    CHECK(ap_owned_object_info(s,(struct ap_program_id){1,101},&m)==0 && m.source==AP_OWNED_PROGRAM_LINK);
    CHECK(ap_owned_object_info(s,(struct ap_program_id){1,106},&m)==0 && m.source==AP_OWNED_DIRECT_FD);
    df_closed(s,0);
}
static void dm_faults(void) {
    struct ap_session *s=NULL;dm_ready(&s);dm_prepare(s);struct ap_stream_fault_state out={0};
    CHECK(!ap_original_read_fault_snapshot(s,1,324,&out) && dm_fault_reads==2 && !memcmp(&out,&dm_fault,sizeof(out)));
    CHECK(ap_original_read_fault_snapshot(s,1,323,&out)==-1);
    df_fault=DF_FAULT_MAP;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);df_fault=DF_OK;
    dm_unstable=true;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);dm_unstable=false;
    struct ap_pending_command *p=&s->pending[ap_command_slot(1)];
    p->submitted.command+=AP_COMMAND_SLOTS;
    CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);p->submitted.command=1;
    p->original_collected=false;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);p->original_collected=true;
    p->original_receipt.selected_file++;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);p->original_receipt.selected_file--;
    p->state=AP_SLOT_FREE;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);p->state=AP_SLOT_COLLECTED;
    p->original_selected=false;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);p->original_selected=true;
    u64 *fields[]={&dm_fault.provider,&dm_fault.command,&dm_fault.call,&dm_fault.task,&dm_fault.start,&dm_fault.file,&dm_fault.pointer,&dm_fault.ubuf};
    for(unsigned i=0;i<sizeof(fields)/sizeof(fields[0]);i++) {
        (*fields[i])++;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);(*fields[i])--;
    }
    dm_info_short=true;CHECK(ap_original_read_fault_snapshot(s,1,324,&out)==-1);dm_info_short=false;
    CHECK(!ap_original_read_fault_snapshot(s,1,324,&out));
    df_closed(s,0);
}
static void dm_gate(void) {
    struct nr_open_observation v={.start=100,.deadline=200,.run_deadline=2000,.returned=150};
    CHECK(!nr_open_observation_result(&v,190,1));
    CHECK(nr_open_timely(200,200,2000)==-1 && errno==ETIMEDOUT);
    CHECK(nr_open_observation_result(&v,200,1)==-1 && errno==ETIMEDOUT);
    v.returned=201;CHECK(nr_open_observation_result(&v,210,1)==-1 && errno==ETIMEDOUT);
    v.open_rc=-1;v.open_error=EACCES;
    CHECK(nr_open_observation_result(&v,210,1)==-1 && errno==EACCES && v.gate_error==ETIMEDOUT);
    v.open_rc=0;v.open_error=0;v.returned=150;v.inventory_rc=-1;v.inventory_error=EIO;
    CHECK(nr_open_observation_result(&v,190,1)==-1 && errno==EIO);
    v.inventory_rc=0;v.inventory_error=0;
    CHECK(nr_open_observation_result(&v,190,0)==-1 && errno==EPROTO);
    CHECK(nr_open_timely(0,200,2000)==-1);
}
int main(int argc,char **argv) {
    if(argc==1 || (argc==2 && !strcmp(argv[1],"all"))) {
        int result=inherited_main(argc,argv);
        assert(!result);df_terminal_cases("all");return result;
    }
    if(argc==2 && !strncmp(argv[1],"terminal-",9)) {
        df_terminal_cases(argv[1]+9);return 0;
    }
    if(argc==1 || (argc==2 && strncmp(argv[1],"owned-",6)))return inherited_main(argc,argv);
    assert(argc==2);const char *selector=argv[1]+6;
    if(!strcmp(selector,"old-id")) {
        int result=object_fd((struct ap_program_id){0,301});
        assert(dm_global_id_queries==1 && errno==EPERM);
        assert(result>=0); /* unchanged required old inventory lookup success */
        return 0;
    }
    if(!strcmp(selector,"inventory") || !strcmp(selector,"all"))dm_inventory();
    if(!strcmp(selector,"identity") || !strcmp(selector,"all"))dm_identity();
    if(!strcmp(selector,"fault") || !strcmp(selector,"all"))dm_faults();
    if(!strcmp(selector,"gate") || !strcmp(selector,"all"))dm_gate();
    assert(dm_checks);printf("owned metadata: %u checks, real driver / modeled kernel, native UNRUN\n",dm_checks);return 0;
}
