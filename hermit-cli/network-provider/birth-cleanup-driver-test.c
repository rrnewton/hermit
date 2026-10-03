/* SPDX-License-Identifier: MIT */
/* Production driver entry points and their actual read/disarm/ACK order.
 * Map/poll and load-time libbpf boundaries are modeled; target-note inputs
 * are fixtures. This is not kernel qualification. */
#define _GNU_SOURCE
#include <assert.h>
#include <stdio.h>
#define fopen ownership_kernel_input
static FILE *ownership_kernel_input(const char *,const char *);
#include "driver.c"
#undef fopen

/* Exercise the actual admission query, including the host kernel's
 * count/address-buffer contract. This models metadata only, not BPF hooks. */
static unsigned info_calls,info_multi_calls,info_copy_calls;
static int info_bad,info_fail_at;
static bool ownership_mode;
static int ownership_info(int,void *,unsigned int *);
int bpf_obj_get_info_by_fd(int fd,void *out,unsigned int *size) {
    if(ownership_mode)return ownership_info(fd,out,size);
    if((fd>=321 && fd<=328) || fd==330 || fd==332) {
        unsigned which=fd==330?4:fd==332?5:(unsigned)(fd-321)/2;
        if(fd&1) {
            assert(*size==sizeof(struct bpf_prog_info));
            struct bpf_prog_info *v=out;v->id=fd;
            v->type=BPF_PROG_TYPE_KPROBE;
        } else {
            assert(*size==sizeof(struct bpf_link_info));
            struct bpf_link_info *v=out;v->id=fd;v->prog_id=fd>=330?fd-5:fd-1;
            v->type=which<2?BPF_LINK_TYPE_KPROBE_MULTI:BPF_LINK_TYPE_PERF_EVENT;
            if(which<2) {
                assert(v->kprobe_multi.count==4 && v->kprobe_multi.addrs && v->kprobe_multi.cookies);
                u64 *cookies=(u64 *)(uintptr_t)v->kprobe_multi.cookies;
                cookies[0]=1;cookies[1]=2;cookies[2]=3;cookies[3]=4;
                v->kprobe_multi.flags=which?BPF_F_KPROBE_MULTI_RETURN:0;
            } else {
                assert(v->perf_event.kprobe.func_name && v->perf_event.kprobe.name_len==64);
                memcpy((void *)(uintptr_t)v->perf_event.kprobe.func_name,AP_STREAM_COPY_SYMBOL,sizeof(AP_STREAM_COPY_SYMBOL));
                v->perf_event.type=BPF_PERF_EVENT_KPROBE;
                v->perf_event.kprobe.name_len=sizeof(AP_STREAM_COPY_SYMBOL);
                const u64 offsets[]={AP_STREAM_COPY_ENTRY_OFFSET,AP_STREAM_COPY_EXIT_OFFSET,
                    AP_STREAM_COPY_FRAG_ENTRY_OFFSET,AP_STREAM_COPY_FRAG_EXIT_OFFSET};
                v->perf_event.kprobe.offset=offsets[which-2];v->perf_event.kprobe.cookie=which+3;
            }
        }
        return 0;
    }
    info_calls++;
    if((int)info_calls==info_fail_at) {errno=EIO;return -1;}
    if(fd==201 || fd==203 || fd==205 || fd==207 || fd==209) {
        assert(*size==sizeof(struct bpf_prog_info));
        struct bpf_prog_info *p=out;
        assert(!p->type && !p->id && !p->recursion_misses);
        p->type=BPF_PROG_TYPE_KPROBE;p->id=(unsigned)fd;
        if(fd==201 && info_bad==1)p->recursion_misses=1;
        return 0;
    }
    assert(*size==sizeof(struct bpf_link_info));
    struct bpf_link_info *l=out;
    if(fd==202) {
        info_multi_calls++;
        /* Exact kernel opening guard, before metadata/cookie copyout. */
        if((!l->kprobe_multi.addrs)!=(!l->kprobe_multi.count)) {errno=EINVAL;return -1;}
        assert(l->kprobe_multi.count==7 && l->kprobe_multi.cookies);
        assert(l->kprobe_multi.addrs!=l->kprobe_multi.cookies);
        u64 *addresses=(u64 *)(uintptr_t)l->kprobe_multi.addrs;
        u64 *cookies=(u64 *)(uintptr_t)l->kprobe_multi.cookies;
        for(unsigned i=0;i<7;i++)assert(addresses[i]==0 && cookies[i]==0);
        /* Restricted kallsyms leaves addresses masked; cookies still identify
         * the owned attachment roles. No nonzero-address exemption is added. */
        cookies[0]=4;cookies[1]=6;cookies[2]=2;cookies[3]=7;cookies[4]=10;cookies[5]=11;cookies[6]=17;
        l->type=BPF_LINK_TYPE_KPROBE_MULTI;l->id=202;l->prog_id=201;
        if(info_bad==2)l->kprobe_multi.count=2;
        if(info_bad==3)cookies[2]=4;
        if(info_bad==4)cookies[2]=63;
        if(info_bad==5)l->prog_id=200;
        if(info_bad==6)l->kprobe_multi.flags=1;
        if(info_bad==7)l->kprobe_multi.missed=1;
        if(info_bad==8)cookies[4]=0;
        if(info_bad==9)cookies[5]=0;
        if(info_bad==10)cookies[5]=10;
        if(info_bad==11)l->kprobe_multi.count=8; /* extra unbound member must refuse */
        if(info_bad==12)cookies[6]=0;
        if(info_bad==13)cookies[6]=11;
        if(info_bad==14)l->kprobe_multi.count=6; /* prior six-site package must refuse */
        if(info_bad==15)*size=offsetof(struct bpf_link_info,kprobe_multi.cookies);
        return 0;
    }
    assert(fd==204 || fd==206 || fd==208 || fd==210 || fd==212 || fd==214 || fd==216 || fd==218 || fd==220 || fd==222 || fd==224);info_copy_calls++;
    assert(l->perf_event.kprobe.func_name && l->perf_event.kprobe.name_len==64);
    const char *symbol=(fd==222 || fd==224)?"do_epoll_ctl":fd==216?"__x64_sys_read":(fd==218 || fd==220)?"fdget_pos":(fd==212 || fd==214)?"fdget_raw":fd==208?"__sys_accept4":"__sys_connect";
    memcpy((void *)(uintptr_t)l->perf_event.kprobe.func_name,symbol,strlen(symbol)+1);
    l->type=BPF_LINK_TYPE_PERF_EVENT;l->id=(unsigned)fd;l->prog_id=fd>=212?209:(unsigned)fd-1;
    l->perf_event.type=BPF_PERF_EVENT_KPROBE;
    l->perf_event.kprobe.name_len=(u32)strlen(symbol)+1;
    l->perf_event.kprobe.offset=fd==204?0x41:fd==206?0x46:fd==208?0x21:fd==212?0x7c:fd==214?0x5:fd==216?0x13:fd==218?0x96:fd==220?0xfa:fd==222?0x23:fd==224?0x37:0x1c;
    l->perf_event.kprobe.cookie=fd==204?3:fd==206?5:fd==208?8:fd==212?13:fd==214?12:fd==216?14:fd==218?15:fd==220?16:fd==222?18:fd==224?19:9;
    return 0;
}
static void observer_readiness_controls(void) {
    struct ap_session s={.fdget_program=201,.fdget_link=202,
        .copy_program={203,205},.copy_link={204,206},
        .selection_program={207,209,209,209,209,209,209,209,209},.selection_link={208,210,212,214,216,218,220,222,224}};
    info_calls=info_multi_calls=info_copy_calls=0;
    assert(fd_accept_observer_ready(&s)==0);
    assert(info_calls==24 && info_multi_calls==1 && info_copy_calls==11);
    for(info_bad=1;info_bad<=7;info_bad++) {
        info_calls=info_multi_calls=info_copy_calls=0;errno=0;
        assert(fd_accept_observer_ready(&s)==-1 && errno==ENODATA);
        assert(info_calls==2 && info_multi_calls==1 && info_copy_calls==0);
    }
    for(info_bad=8;info_bad<=15;info_bad++) {
        info_calls=info_multi_calls=info_copy_calls=0;errno=0;
        assert(fd_accept_observer_ready(&s)==-1 && errno==ENODATA);
        assert(info_calls==2 && info_multi_calls==1 && info_copy_calls==0);
    }
    info_bad=0;
    for(info_fail_at=1;info_fail_at<=24;info_fail_at++) {
        info_calls=info_multi_calls=info_copy_calls=0;errno=0;
        assert(fd_accept_observer_ready(&s)==-1 && errno==EIO);
        assert(info_calls==(unsigned)info_fail_at);
    }
    info_fail_at=0;
    for(unsigned which=0;which<9;which++) {
        int held=s.selection_program[which];s.selection_program[which]=-1;
        info_calls=info_multi_calls=info_copy_calls=0;errno=0;
        assert(fd_accept_observer_ready(&s)==-1 && errno==ENODATA);
        s.selection_program[which]=held;
        held=s.selection_link[which];s.selection_link[which]=-1;
        info_calls=info_multi_calls=info_copy_calls=0;errno=0;
        assert(fd_accept_observer_ready(&s)==-1 && errno==ENODATA);
        s.selection_link[which]=held;
    }
    puts("production observer readiness: masked-address admission,15 metadata refusals,24 syscall failures");
}

/* The real open/identifier/close functions, with libbpf boundaries modeled
 * and target-file fixtures. No BPF runs and no numeric FD is closed here. */
struct bpf_object { unsigned programs,maps; };
struct bpf_program { unsigned at;int fd; };
struct bpf_link { unsigned at;int fd; };
static struct bpf_link metadata_stream_links[6];
struct bpf_map { unsigned at;int fd; };
struct ring { unsigned long consumer,producer; };
struct ring_buffer { bool owned;struct ring ring; };
static struct ring_buffer ownership_ring,empty_test_ring;
struct ring_buffer *ring_buffer__new(int fd,int (*callback)(void *,void *,size_t),void *context,const void *options) {
    assert(ownership_mode && fd==3022 && callback==stream_copy_record && context && !options);
    assert(!ownership_ring.owned);ownership_ring.owned=true;return &ownership_ring;
}
struct ring *ring_buffer__ring(struct ring_buffer *r,unsigned int index) {assert(r && !index);return &r->ring;}
unsigned long ring__consumer_pos(const struct ring *r) {return r->consumer;}
unsigned long ring__producer_pos(const struct ring *r) {return r->producer;}
int ring_buffer__consume(struct ring_buffer *ring) {assert(ring);return 0;}
int ring_buffer__epoll_fd(const struct ring_buffer *ring) {assert(ring);return 4000;}
void ring_buffer__free(struct ring_buffer *ring) {
    assert(ring==&ownership_ring && ring->owned);ring->owned=false;
}
static struct bpf_object ownership_object;
static struct bpf_program ownership_programs[AP_PROGRAMS];
static struct bpf_link ownership_links[AP_LINKS];
static struct bpf_map ownership_maps[24];
static unsigned ownership_unloads,ownership_link_closes,ownership_program_closes,ownership_map_closes;
static unsigned ownership_links_alive,ownership_queries,ownership_checks;
static unsigned ownership_attach_limit,ownership_perf_alive,ownership_kernel_reads,ownership_peak;
static bool ownership_program_queried[AP_PROGRAMS],ownership_link_queried[AP_LINKS];
static unsigned ownership_live_fds(void);
static void ownership_note_peak(void) {
    unsigned fds=12+ownership_live_fds();assert(fds<=128);
    if(fds>ownership_peak)ownership_peak=fds;
}
static bool ownership_loaded,ownership_btf_owned;
static unsigned ownership_bad_at=7;
enum ownership_fault {
    OWN_OK,OWN_PROGRAM_ID,OWN_ZERO_PROGRAM,OWN_SHORT_PROGRAM,OWN_PROGRAM_IO,
    OWN_ZERO_LINK,OWN_ZERO_LINK_PROGRAM,OWN_SHORT_LINK,OWN_LINK_IO,
    OWN_WRONG_LINK_PROGRAM,OWN_DUP_PROGRAM,OWN_DUP_LINK,OWN_ZERO_LINK_TYPE,
    OWN_CHANGED_LINK_TYPE,OWN_CHANGED_LINK_ID,OWN_SHORT_MAP,OWN_MAP_IO,OWN_MISSES,
    OWN_FILE_COOKIE,OWN_FILE_OFFSET,OWN_FILE_SYMBOL,
    OWN_STREAM_COUNT,OWN_STREAM_FLAGS,OWN_STREAM_COOKIE,OWN_STREAM_LINK_MISSES,
    OWN_STREAM_ATTACH,OWN_STREAM_OFFSET,OWN_STREAM_SYMBOL,OWN_STREAM_CLASSIC_COOKIE,
    OWN_STREAM_NAME_LENGTH,OWN_STREAM_CLASSIC_MISSES,OWN_STREAM_SHORT_CLASSIC
};
static enum ownership_fault ownership_fault;
#define OWN_CHECK(value) do {assert(value);ownership_checks++;} while(0)
static int ownership_program_fd(unsigned at) {return at<2?201+(int)at*2:1000+(int)at;}
static int ownership_link_fd(unsigned at) {return at<2?202+(int)at*2:2000+(int)at;}
/* Independent fixture inventory: original Connect first, every preserved
 * former standalone site, File/Read sites, then both actual Ctl fdget sites. */
static const struct {const char *symbol;u64 offset,cookie;} ownership_classic[]={
    {"__sys_connect",0x1c,9},{"__sys_connect",0x41,3},{"__sys_connect",0x46,5},
    {"__sys_accept4",0x21,8},{"fdget_raw",0x7c,13},{"fdget_raw",0x5,12},
    {"__x64_sys_read",0x13,14},{"fdget_pos",0x96,15},{"fdget_pos",0xfa,16},
    {"do_epoll_ctl",0x23,18},{"do_epoll_ctl",0x37,19},
};
static unsigned ownership_classic_index(unsigned at) {
    assert(at==1 || (at>=AP_PROGRAMS && at<AP_PROGRAMS+10));
    return at==1?0:at-AP_PROGRAMS+1;
}
static int ownership_info(int fd,void *out,unsigned int *size) {
    ownership_queries++;
    for(unsigned at=0;at<ownership_object.programs;at++)if(fd==ownership_program_fd(at)) {
        assert(ownership_programs[at].fd==fd); /* no stale query after unload */
        assert(*size==sizeof(struct bpf_prog_info));
        if(at==ownership_bad_at && ownership_fault==OWN_PROGRAM_IO) {errno=EIO;return -1;}
        struct bpf_prog_info *p=out;assert(!p->id && !p->type && !p->recursion_misses);
        p->id=10000+at;p->type=(at<4 || at>=42)?BPF_PROG_TYPE_KPROBE:BPF_PROG_TYPE_TRACING;
        if(at==ownership_bad_at) {
            if(ownership_fault==OWN_PROGRAM_ID)p->id++;
            if(ownership_fault==OWN_ZERO_PROGRAM)p->id=0;
            if(ownership_fault==OWN_SHORT_PROGRAM)*size=offsetof(struct bpf_prog_info,id);
            if(ownership_fault==OWN_DUP_PROGRAM)p->id--;
            if(ownership_fault==OWN_MISSES)p->recursion_misses=1;
        }
        ownership_program_queried[at]=true;return 0;
    }
    for(unsigned at=0;at<AP_LINKS;at++)if(fd==ownership_link_fd(at)) {
        assert(ownership_links[at].fd==fd && at<ownership_links_alive);
        assert(*size==sizeof(struct bpf_link_info));
        if(at==ownership_bad_at && ownership_fault==OWN_LINK_IO) {errno=EIO;return -1;}
        struct bpf_link_info *l=out;
        assert(!l->type && !l->id && !l->prog_id);
        l->id=10000+at;l->prog_id=10000+(at>=AP_PROGRAMS+10?44+at-(AP_PROGRAMS+10):at>=AP_PROGRAMS?1:at);
        l->type=(at==0 || at==2 || at==3 || at==42 || at==43)?BPF_LINK_TYPE_KPROBE_MULTI:(at==1 || at>=44)?BPF_LINK_TYPE_PERF_EVENT:BPF_LINK_TYPE_TRACING;
        if(at==0) {
            if((!l->kprobe_multi.addrs)!=(!l->kprobe_multi.count)) {errno=EINVAL;return -1;}
            if(l->kprobe_multi.count) {
                assert(l->kprobe_multi.count==7 && l->kprobe_multi.cookies);
                u64 *addresses=(u64 *)(uintptr_t)l->kprobe_multi.addrs;
                u64 *cookies=(u64 *)(uintptr_t)l->kprobe_multi.cookies;
                for(unsigned i=0;i<7;i++)assert(!addresses[i] && !cookies[i]);
                cookies[0]=4;cookies[1]=6;cookies[2]=2;cookies[3]=7;cookies[4]=10;cookies[5]=11;cookies[6]=17;
            }
            l->kprobe_multi.count=7;
        } else if(at==2 || at==3) {
            /* Same explicit one-site multi attachment as the production loader. */
            l->kprobe_multi.count=1;
        } else if(at==42 || at==43) {
            if((!l->kprobe_multi.addrs)!=(!l->kprobe_multi.count)) {errno=EINVAL;return -1;}
            if(l->kprobe_multi.count) {
                assert(l->kprobe_multi.count==4 && l->kprobe_multi.cookies);
                u64 *cookies=(u64 *)(uintptr_t)l->kprobe_multi.cookies;
                cookies[0]=1;cookies[1]=2;cookies[2]=3;cookies[3]=4;
                if(at==ownership_bad_at && ownership_fault==OWN_STREAM_COOKIE)cookies[2]=1;
            }
            l->kprobe_multi.count=4;l->kprobe_multi.flags=at==43?BPF_F_KPROBE_MULTI_RETURN:0;
            if(at==ownership_bad_at) {
                if(ownership_fault==OWN_STREAM_COUNT)l->kprobe_multi.count=2;
                if(ownership_fault==OWN_STREAM_FLAGS)l->kprobe_multi.flags^=BPF_F_KPROBE_MULTI_RETURN;
                if(ownership_fault==OWN_STREAM_LINK_MISSES)l->kprobe_multi.missed=1;
            }
        } else if(at==44 || at==45 || at>=AP_PROGRAMS+10) {
            assert((!l->perf_event.kprobe.func_name)==(!l->perf_event.kprobe.name_len));
            if(l->perf_event.kprobe.func_name) {
                assert(l->perf_event.kprobe.name_len==64);
                const char *symbol=at==ownership_bad_at && (ownership_fault==OWN_STREAM_SYMBOL || ownership_fault==OWN_FILE_SYMBOL)?
                    "simple_copy_to_iter":AP_STREAM_COPY_SYMBOL;
                memcpy((void *)(uintptr_t)l->perf_event.kprobe.func_name,symbol,strlen(symbol)+1);
            }
            l->perf_event.type=BPF_PERF_EVENT_KPROBE;
            l->perf_event.kprobe.name_len=sizeof(AP_STREAM_COPY_SYMBOL);
            unsigned which=at>=AP_PROGRAMS+10?at-(AP_PROGRAMS+10)+2:at-44;
            const u64 offsets[]={AP_STREAM_COPY_ENTRY_OFFSET,AP_STREAM_COPY_EXIT_OFFSET,
                AP_STREAM_COPY_FRAG_ENTRY_OFFSET,AP_STREAM_COPY_FRAG_EXIT_OFFSET};
            l->perf_event.kprobe.offset=offsets[which];l->perf_event.kprobe.cookie=5+which;
            if(at==ownership_bad_at) {
                if(ownership_fault==OWN_STREAM_ATTACH)l->perf_event.type=BPF_PERF_EVENT_UPROBE;
                if(ownership_fault==OWN_STREAM_OFFSET)l->perf_event.kprobe.offset=1;
                if(ownership_fault==OWN_STREAM_CLASSIC_COOKIE)l->perf_event.kprobe.cookie=0;
                if(ownership_fault==OWN_STREAM_NAME_LENGTH)l->perf_event.kprobe.name_len--;
                if(ownership_fault==OWN_STREAM_CLASSIC_MISSES)l->perf_event.kprobe.missed=1;
                if(ownership_fault==OWN_STREAM_SHORT_CLASSIC)*size=offsetof(struct bpf_link_info,perf_event.kprobe.missed);
            }
        } else if(at==1 || at>=AP_PROGRAMS) {
            unsigned site=ownership_classic_index(at);
            assert((!l->perf_event.kprobe.func_name)==(!l->perf_event.kprobe.name_len));
            if(l->perf_event.kprobe.func_name) {
                assert(l->perf_event.kprobe.name_len==64);
                const char *symbol=ownership_classic[site].symbol;
                if(at==ownership_bad_at && ownership_fault==OWN_FILE_SYMBOL)symbol="unbound_symbol";
                memcpy((void *)(uintptr_t)l->perf_event.kprobe.func_name,symbol,strlen(symbol)+1);
            }
            l->perf_event.type=BPF_PERF_EVENT_KPROBE;
            l->perf_event.kprobe.name_len=(u32)strlen(ownership_classic[site].symbol)+1;
            l->perf_event.kprobe.offset=ownership_classic[site].offset;
            l->perf_event.kprobe.cookie=ownership_classic[site].cookie;
        }
        if(at==ownership_bad_at) {
            if(ownership_fault==OWN_ZERO_LINK)l->id=0;
            if(ownership_fault==OWN_ZERO_LINK_PROGRAM)l->prog_id=0;
            if(ownership_fault==OWN_SHORT_LINK)*size=offsetof(struct bpf_link_info,prog_id);
            if(ownership_fault==OWN_WRONG_LINK_PROGRAM)l->prog_id++;
            if(ownership_fault==OWN_DUP_PROGRAM)l->prog_id--;
            if(ownership_fault==OWN_DUP_LINK)l->id--;
            if(ownership_fault==OWN_ZERO_LINK_TYPE)l->type=0;
            if(ownership_fault==OWN_CHANGED_LINK_TYPE)l->type=BPF_LINK_TYPE_CGROUP;
            if(ownership_fault==OWN_CHANGED_LINK_ID)l->id++;
            if(ownership_fault==OWN_FILE_COOKIE)l->perf_event.kprobe.cookie=9;
            if(ownership_fault==OWN_FILE_OFFSET)l->perf_event.kprobe.offset=0x1c;
        }
        ownership_link_queried[at]=true;return 0;
    }
    for(unsigned at=0;at<ownership_object.maps;at++)if(fd==3000+(int)at) {
        assert(ownership_maps[at].fd==fd && *size==sizeof(struct bpf_map_info));
        if(at==ownership_bad_at && ownership_fault==OWN_MAP_IO) {errno=EIO;return -1;}
        struct bpf_map_info *m=out;assert(!m->id);m->id=10000+at;
        if(at==1) { /* Actual tasks-map FD returned by this load facade. */
            m->type=BPF_MAP_TYPE_TASK_STORAGE;m->key_size=sizeof(int);
            m->value_size=sizeof(struct ap_task_command);m->map_flags=BPF_F_NO_PREALLOC;
        }
        if(at==23) { /* Added command-owned executable observation sidecar. */
            m->type=BPF_MAP_TYPE_ARRAY;m->key_size=sizeof(u32);
            m->value_size=sizeof(struct ap_executable_source);m->max_entries=AP_COMMANDS;
        }
        if(at==ownership_bad_at && ownership_fault==OWN_SHORT_MAP)*size=offsetof(struct bpf_map_info,id);
        return 0;
    }
    assert(0 && "unexpected ownership metadata FD");return -1;
}

struct copy_image_fixture { off64_t position;long corrupt,truncate; };
static ssize_t copy_image_read(void *opaque,char *out,size_t size) {
    struct copy_image_fixture *fixture=opaque;
    if(fixture->truncate>=0 && fixture->position>=fixture->truncate)return 0;
    for(size_t i=0;i<sizeof(ap_copy_image_slices)/sizeof(ap_copy_image_slices[0]);i++) {
        const struct ap_copy_image_slice *slice=&ap_copy_image_slices[i];
        if(fixture->position<slice->offset || fixture->position>=slice->offset+(off64_t)slice->size)continue;
        size_t at=fixture->position-slice->offset;
        if(size>slice->size-at)size=slice->size-at;
        if(fixture->truncate>=0 && size>(size_t)(fixture->truncate-fixture->position))size=fixture->truncate-fixture->position;
        memcpy(out,slice->bytes+at,size);
        if(fixture->corrupt>=fixture->position && fixture->corrupt<fixture->position+(off64_t)size)
            out[fixture->corrupt-fixture->position]^=1;
        fixture->position+=size;return size;
    }
    return 0;
}
static int copy_image_seek(void *opaque,off64_t *offset,int whence) {
    struct copy_image_fixture *fixture=opaque;
    if(whence!=SEEK_SET || *offset<0) {errno=EINVAL;return -1;}
    fixture->position=*offset;return 0;
}
static FILE *copy_image_fixture_open(struct copy_image_fixture *fixture) {
    FILE *input=fopencookie(fixture,"r",(cookie_io_functions_t){.read=copy_image_read,.seek=copy_image_seek});
    if(input)assert(!setvbuf(input,NULL,_IONBF,0));return input;
}
static FILE *ownership_kernel_input(const char *path,const char *mode) {
    assert(ownership_mode && !strcmp(mode,"re"));ownership_kernel_reads++;
    /* Exact positive parser inputs, not a bypass of the production target
     * check and not an assertion about the machine running this pure test. */
    static unsigned char notes[]={4,0,0,0,20,0,0,0,3,0,0,0,'G','N','U',0,
        0xc9,0x40,0x78,0x92,0xac,0xd3,0x01,0x14,0x61,0x91,0xb0,0x0f,0x42,0x3f,0x26,0x12,0xbf,0x8b,0x2d,0x6a};
    static char symbols[]="0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n"
        "0000000000000000 t do_close_on_exec\n0000000000000000 t filp_close\n"
        "0000000000000000 t ptrace_request\n";
    if(!strcmp(path,"/sys/kernel/notes"))return fmemopen(notes,sizeof(notes),"r");
    if(!strcmp(path,AP_COPY_KERNEL_IMAGE)) {
        static struct copy_image_fixture image;image=(struct copy_image_fixture){.corrupt=-1,.truncate=-1};
        return copy_image_fixture_open(&image);
    }
    assert(!strcmp(path,"/proc/kallsyms"));return fmemopen(symbols,sizeof(symbols)-1,"r");
}
struct bpf_object *bpf_object__open_file(const char *path,const void *options) {
    assert(ownership_mode && !strcmp(path,"ownership-fixture") && !options);
    return &ownership_object;
}
long libbpf_get_error(const void *pointer) {
    assert(ownership_mode);
    if(!pointer || pointer==&ownership_object)return 0;
    for(unsigned i=0;i<AP_LINKS;i++)if(pointer==&ownership_links[i])return 0;
    assert(0 && "unowned libbpf pointer");return -EINVAL;
}
int bpf_object__load(struct bpf_object *object) {
    assert(ownership_mode && object==&ownership_object && !ownership_loaded);
    ownership_loaded=ownership_btf_owned=true;
    for(unsigned i=0;i<object->programs;i++)ownership_programs[i].fd=ownership_program_fd(i);
    for(unsigned i=0;i<object->maps;i++)ownership_maps[i].fd=3000+(int)i;
    return 0;
}
const char *bpf_program__name(const struct bpf_program *p) {
    static const char *names[]={"fd_accept_selected","fd_connect_post_fdget","fd_file_retired","fd_exec_closed_file"};
    assert(ownership_mode);
    static const char *copy_names[]={"fd_stream_copy_protocol_enter","fd_stream_copy_protocol_exit",
        "fd_stream_copy_enter","fd_stream_copy_exit"};
    return p->at<4?names[p->at]:p->at>=42?copy_names[p->at-42]:"ordinary_program";
}
static struct bpf_link *ownership_attach(const struct bpf_program *p) {
    assert(ownership_mode && ownership_loaded && p->fd>=0);
    unsigned at=ownership_links_alive;
    assert(p->at==at || (at>=AP_PROGRAMS && at<AP_PROGRAMS+10 && p->at==1) ||
        (at>=AP_PROGRAMS+10 && at<AP_LINKS && p->at==44+at-(AP_PROGRAMS+10)));
    if(at==ownership_attach_limit) {errno=EIO;return NULL;}
    ownership_links[at].fd=ownership_link_fd(at);ownership_links_alive++;
    ownership_note_peak();
    return &ownership_links[at];
}
struct bpf_link *bpf_program__attach(const struct bpf_program *p) {
    assert(p->at>=4);return ownership_attach(p);
}
struct bpf_link *bpf_program__attach_kprobe_multi_opts(const struct bpf_program *p,const char *pattern,
    const struct ap_kprobe_multi_opts *options) {
    assert(!pattern && options->sz==sizeof(*options));
    if(p->at==42 || p->at==43) {
        assert(options->cnt==4 && options->cookies && !options->session);
        assert(options->cookies[0]==1 && options->cookies[1]==2 && options->cookies[2]==3 && options->cookies[3]==4);
        assert(options->retprobe==(p->at==43));
        assert(!strcmp(options->syms[0],"inet_recvmsg") && !strcmp(options->syms[1],"inet6_recvmsg")
            && !strcmp(options->syms[2],"unix_stream_recvmsg")
            && !strcmp(options->syms[3],"skb_copy_datagram_iter"));
        return ownership_attach(p);
    }
    assert(!options->retprobe);
    assert((p->at==0 && options->cnt==7 && options->session && options->cookies) ||
           ((p->at==2 || p->at==3) && options->cnt==1 && !options->session && !options->cookies));
    if(p->at==0) {
        assert(!strcmp(options->syms[0],"__sys_accept4") && options->cookies[0]==6);
        assert(!strcmp(options->syms[1],"security_socket_connect") && options->cookies[1]==2);
        assert(!strcmp(options->syms[2],"__audit_sockaddr") && options->cookies[2]==4);
        assert(!strcmp(options->syms[3],"__sys_connect") && options->cookies[3]==7);
        assert(!strcmp(options->syms[4],"f_dupfd") && options->cookies[4]==10);
        assert(!strcmp(options->syms[5],"alloc_fd") && options->cookies[5]==11);
        assert(!strcmp(options->syms[6],"do_epoll_ctl") && options->cookies[6]==17);
    }
    return ownership_attach(p);
}
struct bpf_link *bpf_program__attach_kprobe_opts(const struct bpf_program *p,const char *symbol,
    const struct ap_kprobe_opts *options) {
    if(p->at==44 || p->at==45) {
        assert(!strcmp(symbol,AP_STREAM_COPY_SYMBOL) && options->sz==sizeof(*options));
        unsigned which=(ownership_links_alive>=AP_PROGRAMS+10?2:0)+(p->at-44);
        const u64 offsets[]={AP_STREAM_COPY_ENTRY_OFFSET,AP_STREAM_COPY_EXIT_OFFSET,
            AP_STREAM_COPY_FRAG_ENTRY_OFFSET,AP_STREAM_COPY_FRAG_EXIT_OFFSET};
        assert(!options->retprobe && options->attach_mode==3 && options->offset==offsets[which]);
        assert(options->bpf_cookie==5+which);
        struct bpf_link *link=ownership_attach(p);
        if(link) {ownership_perf_alive++;ownership_note_peak();}return link;
    }
    assert(p->at==1);
    unsigned site=ownership_classic_index(ownership_links_alive);
    assert(!strcmp(symbol,ownership_classic[site].symbol) && options->sz==sizeof(*options) &&
        !options->retprobe && options->attach_mode==3);
    assert(options->offset==ownership_classic[site].offset && options->bpf_cookie==ownership_classic[site].cookie);
    struct bpf_link *link=ownership_attach(p);
    if(link) {ownership_perf_alive++;ownership_note_peak();}return link;
}
struct bpf_program *bpf_object__next_program(const struct bpf_object *object,struct bpf_program *p) {
    assert(ownership_mode && object==&ownership_object);
    unsigned at=p?p->at+1:0;
    return at<object->programs?&ownership_programs[at]:NULL;
}
struct bpf_map *bpf_object__next_map(const struct bpf_object *object,const struct bpf_map *m) {
    assert(ownership_mode && object==&ownership_object);
    unsigned at=m?m->at+1:0;
    return at<object->maps?&ownership_maps[at]:NULL;
}
int bpf_program__fd(const struct bpf_program *p) {
    assert(ownership_mode);return p->fd>=0?p->fd:-ENOENT;
}
int bpf_map__fd(const struct bpf_map *m) {assert(ownership_mode);return m->fd;}
int bpf_link__fd(const struct bpf_link *l) {
    if(!ownership_mode) {
        for(unsigned i=0;i<6;i++)if(l==&metadata_stream_links[i])return l->fd;
        assert(0 && "unowned test link");
    }
    return l->fd;
}
void bpf_program__unload(struct bpf_program *p) {
    /* Exact40 redundant handles: neither the two selected-file readers nor
     * the four stream-copy readers may be unloaded. */
    assert(ownership_mode && p->fd>=0 && p->at>=2 && p->at<42);
    /* This exact pair, not some earlier pair, must be positively queried
     * before unloading. No current/next binding failure can close this FD. */
    assert(ownership_program_queried[p->at] && ownership_link_queried[p->at]);
    assert(ownership_links[p->at].fd>=0 && ownership_links_alive==p->at+1);
    p->fd=-1;ownership_unloads++;
}
int bpf_link__destroy(struct bpf_link *l) {
    assert(ownership_mode && l->fd>=0 && ownership_links_alive && l->at==ownership_links_alive-1);
    /* One original classic dispatcher, two stream-copy sites, all ten
     * shared classic sites and both fragment sites own perf-event handles.
     * File-retire/exec multi links at2/3 never own those handles. */
    if(l->at==1 || l->at==44 || l->at==45 || l->at>=AP_PROGRAMS) {
        assert(ownership_perf_alive);ownership_perf_alive--;
    }
    l->fd=-1;ownership_links_alive--;ownership_link_closes++;return 0;
}
void bpf_object__close(struct bpf_object *object) {
    assert(ownership_mode && object==&ownership_object && !ownership_links_alive && !ownership_perf_alive);
    ownership_loaded=ownership_btf_owned=false;
    for(unsigned i=0;i<object->programs;i++)if(ownership_programs[i].fd>=0) {
        ownership_programs[i].fd=-1;ownership_program_closes++;
    }
    for(unsigned i=0;i<object->maps;i++)if(ownership_maps[i].fd>=0) {
        ownership_maps[i].fd=-1;ownership_map_closes++;
    }
}
static void ownership_reset(unsigned attach_limit) {
    ownership_mode=true;ownership_object=(struct bpf_object){AP_PROGRAMS,24};
    ownership_unloads=ownership_link_closes=ownership_program_closes=ownership_map_closes=ownership_queries=0;
    ownership_fault=OWN_OK;ownership_bad_at=7;ownership_links_alive=0;
    ownership_attach_limit=attach_limit;ownership_perf_alive=ownership_kernel_reads=ownership_peak=0;
    ownership_loaded=ownership_btf_owned=false;assert(!ownership_ring.owned);
    for(unsigned i=0;i<AP_PROGRAMS;i++) {
        ownership_programs[i]=(struct bpf_program){i,-1};
        ownership_program_queried[i]=false;
    }
    for(unsigned i=0;i<AP_LINKS;i++) {
        ownership_links[i]=(struct bpf_link){i,-1};ownership_link_queried[i]=false;
    }
    for(unsigned i=0;i<24;i++)ownership_maps[i]=(struct bpf_map){i,-1};
}
static unsigned ownership_live_fds(void) {
    unsigned count=ownership_btf_owned+ownership_perf_alive+ownership_ring.owned;
    for(unsigned i=0;i<AP_PROGRAMS;i++)count+=ownership_programs[i].fd>=0;
    for(unsigned i=0;i<AP_LINKS;i++)count+=ownership_links[i].fd>=0;
    for(unsigned i=0;i<24;i++)count+=ownership_maps[i].fd>=0;
    return count;
}
static void ownership_inventory(struct ap_session *s,unsigned links) {
    struct ap_program_id ids[128];u32 count=0;
    OWN_CHECK(ap_identifiers(s,ids,128,&count)==0);
    OWN_CHECK(count==24+AP_PROGRAMS+links);
    const unsigned expected[]={24,AP_PROGRAMS,links};
    for(unsigned kind=0;kind<3;kind++)for(unsigned at=0;at<expected[kind];at++) {
        unsigned matches=0;
        for(unsigned i=0;i<count;i++)matches+=ids[i].kind==kind && ids[i].id==10000+at;
        OWN_CHECK(matches==1); /* same numeric IDs in all three namespaces */
    }
}
static void ownership_close(struct ap_session *s,unsigned links,unsigned released) {
    OWN_CHECK(ap_close(s)==0);
    OWN_CHECK(ownership_link_closes==links && !ownership_links_alive);
    OWN_CHECK(ownership_unloads==released && ownership_program_closes+released==AP_PROGRAMS);
    OWN_CHECK(ownership_map_closes==24 && ownership_live_fds()==0);
}
static void program_ownership_controls(void) {
    struct ap_session *s=NULL;ownership_reset(AP_LINKS);
    OWN_CHECK(ap_open("ownership-fixture",3,&s)==0);
    OWN_CHECK(ownership_kernel_reads==3);
    /* Same12 baseline descriptors/four sockets/128 cap. Early bound release
     * admits all58 actual links and the ring epoll FD. Forty redundant load
     * handles are released; six distinct program readers remain. No link or
     * identifier is omitted to meet the unchanged128 bounds. */
    OWN_CHECK(ownership_peak==117);
    OWN_CHECK(12+ownership_live_fds()+4<=128);
    OWN_CHECK(12+ownership_live_fds()+4==121);
    OWN_CHECK(ownership_unloads==40 && ownership_queries==216+2 /* exact task and ABI11 executable-map queries */);
    for(unsigned i=0;i<AP_PROGRAMS;i++) {
        OWN_CHECK(ownership_programs[i].fd==(i<2 || i>=42?ownership_program_fd(i):-1));
    }
    ownership_inventory(s,AP_LINKS);
    for(unsigned i=0;i<2;i++) {
        ownership_fault=OWN_MISSES;ownership_bad_at=i;errno=0;
        OWN_CHECK(fd_accept_observer_ready(s)==-1 && errno==ENODATA);
        OWN_CHECK(ownership_unloads==40);
    }
    ownership_fault=OWN_OK;
    OWN_CHECK(fd_accept_observer_ready(s)==0);
    for(unsigned i=42;i<46;i++) {
        ownership_bad_at=i;ownership_fault=OWN_MISSES;errno=0;
        OWN_CHECK(stream_copy_observer_ready(s)==-1 && errno==ENODATA);
        OWN_CHECK(ownership_programs[i].fd==ownership_program_fd(i));
    }
    const unsigned copy_sites[]={42,43,44,45,AP_PROGRAMS+10,AP_PROGRAMS+11};
    for(unsigned n=0;n<sizeof(copy_sites)/sizeof(copy_sites[0]);n++) {
        unsigned i=copy_sites[n];
        unsigned first=i<44?OWN_STREAM_COUNT:OWN_STREAM_ATTACH;
        unsigned last=i<44?OWN_STREAM_LINK_MISSES:OWN_STREAM_SHORT_CLASSIC;
        for(unsigned bad=first;bad<=last;bad++) {
            ownership_bad_at=i;ownership_fault=(enum ownership_fault)bad;errno=0;
            OWN_CHECK(stream_copy_observer_ready(s)==-1 && errno==ENODATA);
            OWN_CHECK(ownership_unloads==40 && ownership_links_alive==AP_LINKS);
        }
    }
    ownership_fault=OWN_OK;
    OWN_CHECK(stream_copy_observer_ready(s)==0);
    struct ap_program_id ids[128];u32 count=0;errno=0;
    OWN_CHECK(ap_identifiers(s,ids,110,&count)==-1 && errno==EOVERFLOW && count==110);
    count=0;errno=0;OWN_CHECK(ap_identifiers(s,ids,111,&count)==-1 && errno==EOVERFLOW && count==111);
    for(u32 capacity=112;capacity<128;capacity++) {
        count=0;errno=0;
        OWN_CHECK(ap_identifiers(s,ids,capacity,&count)==-1 && errno==EOVERFLOW && count==capacity);
    }
    count=0;OWN_CHECK(ap_identifiers(s,ids,128,&count)==0 && count==128);
    OWN_CHECK(ownership_unloads==40);
    ownership_close(s,AP_LINKS,40);

    /* All original metadata refusals remain selected. Earlier exact-bound
     * pairs may be unloaded; the failing and all unlinked programs stay held. */
    const unsigned preflight_at[]={2,3,4,40,41,42,43,44,45};
    for(unsigned at=0;at<sizeof(preflight_at)/sizeof(preflight_at[0]);at++)
    for(unsigned bad=OWN_PROGRAM_ID;bad<=OWN_ZERO_LINK_TYPE;bad++) {
        ownership_reset(AP_LINKS);ownership_fault=(enum ownership_fault)bad;
        ownership_bad_at=preflight_at[at];errno=0;
        OWN_CHECK(ap_open("ownership-fixture",3,&s)==-1 && s);
        OWN_CHECK(errno==((bad==OWN_PROGRAM_IO || bad==OWN_LINK_IO)?EIO:ENODATA));
        unsigned bound=preflight_at[at],released=bound>42?40:bound>2?bound-2:0;
        OWN_CHECK(ownership_unloads==released);
        for(unsigned i=0;i<AP_PROGRAMS;i++)
            OWN_CHECK(ownership_programs[i].fd==((i>=2 && i<42 && i<bound)?-1:ownership_program_fd(i)));
        ownership_fault=OWN_OK;ownership_inventory(s,bound+1);
        ownership_close(s,bound+1,released);
    }
    /* Every unlinked program stays held; every released linked program has
     * its own queried custody. Preserve all old partial sites and add the
     * every new link failure after the46 distinct program attachments. */
    for(unsigned links=0;links<AP_LINKS;links++) {ownership_reset(links);errno=0;
        OWN_CHECK(ap_open("ownership-fixture",3,&s)==-1 && errno==EIO && s);
        unsigned original=links<AP_PROGRAMS?links:AP_PROGRAMS;
        unsigned released=original>42?40:original>2?original-2:0;
        OWN_CHECK(ownership_unloads==released && ownership_queries==2*links+2 /* exact task and ABI11 executable-map queries */);
        ownership_inventory(s,links);ownership_close(s,links,released);
    }
    /* Every later link/program/map response is fresh. A prior positive binding
     * is not a replacement for a failed, short or contradictory current query. */
    const enum ownership_fault later[]={OWN_ZERO_LINK,OWN_ZERO_LINK_PROGRAM,OWN_SHORT_LINK,
        OWN_LINK_IO,OWN_WRONG_LINK_PROGRAM,OWN_DUP_LINK,OWN_ZERO_LINK_TYPE,
        OWN_CHANGED_LINK_TYPE,OWN_CHANGED_LINK_ID,OWN_SHORT_MAP,OWN_MAP_IO};
    for(unsigned at=0;at<sizeof(later)/sizeof(later[0]);at++) {
        ownership_reset(AP_LINKS);OWN_CHECK(ap_open("ownership-fixture",3,&s)==0);
        ownership_fault=later[at];count=0;errno=0;
        OWN_CHECK(ap_identifiers(s,ids,128,&count)==-1);
        OWN_CHECK(errno==((later[at]==OWN_LINK_IO || later[at]==OWN_MAP_IO)?EIO:ENODATA));
        OWN_CHECK(ownership_unloads==40 && ownership_links_alive==AP_LINKS);
        ownership_fault=OWN_OK;ownership_inventory(s,AP_LINKS);
        ownership_close(s,AP_LINKS,40);
    }
    ownership_reset(AP_LINKS);OWN_CHECK(ap_open("ownership-fixture",3,&s)==0);
    ownership_bad_at=0;ownership_fault=OWN_PROGRAM_ID;errno=0;
    OWN_CHECK(ap_identifiers(s,ids,128,&count)==-1 && errno==ENODATA);
    ownership_fault=OWN_OK;ownership_inventory(s,AP_LINKS);
    ownership_close(s,AP_LINKS,40);
    /* The extra physical link must match the same retained program and
     * its own exact site; it is not exempt from identity or missed checks. */
    const enum ownership_fault extra_bad[]={OWN_ZERO_LINK,OWN_ZERO_LINK_PROGRAM,OWN_SHORT_LINK,
        OWN_LINK_IO,OWN_WRONG_LINK_PROGRAM,OWN_DUP_LINK,OWN_ZERO_LINK_TYPE,
        OWN_FILE_COOKIE,OWN_FILE_OFFSET,OWN_FILE_SYMBOL};
    for(unsigned site=AP_PROGRAMS;site<AP_LINKS;site++)
    for(unsigned i=0;i<sizeof(extra_bad)/sizeof(extra_bad[0]);i++) {
        ownership_reset(AP_LINKS);ownership_bad_at=site;ownership_fault=extra_bad[i];errno=0;
        OWN_CHECK(ap_open("ownership-fixture",3,&s)==-1 && s);
        OWN_CHECK(errno==(extra_bad[i]==OWN_LINK_IO?EIO:ENODATA));
        unsigned loaded=extra_bad[i]>=OWN_FILE_COOKIE?AP_LINKS:site+1;
        OWN_CHECK(ownership_unloads==40 && ownership_links_alive==loaded);
        ownership_fault=OWN_OK;ownership_inventory(s,loaded);ownership_close(s,loaded,40);
    }
    ownership_mode=false;
    printf("production program FD ownership: %u checks;128 objects,40 released load handles,6 retained program handles\n",ownership_checks);
}

enum { TASKS=101, COMMANDS, STATUS, CALLS, FD_STATUS };
static struct ap_task_command task;
static struct ap_command_result command;
static struct ap_fd_call row;
static bool task_absent,row_absent,dead,child_absent;
static unsigned task_reads,row_reads,updates,deletes,idle_writes;
static bool die_at_task_read,die_after_row_read,die_at_idle_write;
static bool fail_task_delete_after_effect,fail_idle_after_effect,fail_row_delete_after_effect;
static bool torn_command,torn_row;
static unsigned command_reads;
static int poll_failure;
enum late_race { LATE_NONE, LATE_UPDATE_MISSING, LATE_DELETE_MISSING, LATE_READBACK_DEATH };
static enum late_race late_race;
static bool late_boundary,late_hup,allow_hup;
static int late_rc,late_errno,missing_rc,missing_errno,poll_errno_clobber;
static unsigned late_polls,late_lookups,task_update_effects,task_delete_effects;
static int late_lookup_failure,late_poll_bits;
static bool detach_after_delete;
static void late_detach(void) {
    late_boundary=true;dead=task_absent=true;late_hup=allow_hup;
}
int poll(struct pollfd *fds,nfds_t n,int timeout) {
    assert(n==1 && timeout==0 && fds[0].events==POLLIN);
    if(poll_failure) {errno=poll_failure;return -1;}
    fds[0].revents=dead && fds[0].fd==9?POLLIN:0;
    if(late_boundary) {
        late_polls++;
        if(late_hup)fds[0].revents|=POLLHUP;
        if(late_poll_bits)fds[0].revents=late_poll_bits;
    }
    if(poll_errno_clobber)errno=poll_errno_clobber;
    return fds[0].revents?1:0;
}
int bpf_object__find_map_fd_by_name(const struct bpf_object *object,const char *name) {
    if(ownership_mode) {
        assert(object==&ownership_object && ownership_loaded);
        if(!strcmp(name,"stream_copy_records"))return 3022;
        if(!strcmp(name,"executable_sources"))return 3023;
        static const char *names[]={"ap_config_map","tasks","commands","events","status"};
        for(unsigned i=0;i<5;i++)if(!strcmp(name,names[i]))return 3000+(int)i;
        assert(0 && "unexpected load map lookup");return -ENOENT;
    }
    (void)object;
    if(!strcmp(name,"fd_calls"))return CALLS;
    if(!strcmp(name,"fd_status"))return FD_STATUS;
    errno=ENOENT;return -1;
}
int bpf_map_lookup_elem(int map,const void *key,void *out) {
    if(map==TASKS) {
        if(*(const int *)key==12) {
            if(child_absent) {errno=ENOENT;return -1;}
            memcpy(out,&task,sizeof(task));return 0;
        }
        assert(*(const int *)key==9);
        task_reads++;
        if(late_polls) {
            late_lookups++;
            if(late_lookup_failure==1) {memcpy(out,&task,sizeof(task));return 0;}
            if(late_lookup_failure==2) {
                memcpy(out,&task,sizeof(task));((struct ap_task_command *)out)->generation_after++;
                return 0;
            }
            if(late_lookup_failure==3) {errno=EIO;return -EIO;}
            if(late_lookup_failure==4) {errno=ENOENT;return -1;}
        }
        if(die_at_task_read) {dead=task_absent=true;die_at_task_read=false;}
        if(task_absent) {errno=missing_errno;return missing_rc;}
        memcpy(out,&task,sizeof(task));return 0;
    }
    if(map==COMMANDS) {
        assert(*(const u32 *)key==7);command_reads++;
        memcpy(out,&command,sizeof(command));
        if(torn_command && command_reads==2)((struct ap_command_result *)out)->start_boottime++;
        return 0;
    }
    if(map==CALLS) {
        const struct ap_invocation_key *identity=key;
        assert(identity->task==command.task && identity->start==command.start_boottime);
        if(row_absent) {errno=ENOENT;return -1;}
        row_reads++;memcpy(out,&row,sizeof(row));
        if(torn_row && row_reads==2)((struct ap_fd_call *)out)->birth.child_start++;
        if(die_after_row_read && row_reads==2) {dead=task_absent=true;}
        return 0;
    }
    if(map==STATUS) {memset(out,0,sizeof(struct ap_status));return 0;}
    assert(map==FD_STATUS);memset(out,0,sizeof(struct ap_fd_status));return 0;
}
int bpf_map_update_elem(int map,const void *key,const void *value,unsigned long long flags) {
    if(ownership_mode) {
        assert(map==3000 && flags==BPF_ANY && *(const u32 *)key==0);
        assert(((const struct ap_config *)value)->provider==3);return 0;
    }
    assert(flags==BPF_EXIST);updates++;
    if(map==TASKS) {
        assert(*(const int *)key==9);idle_writes++;
        if(die_at_idle_write) {dead=task_absent=true;errno=ESRCH;return -1;}
        if(late_race==LATE_UPDATE_MISSING) {late_detach();errno=late_errno;return late_rc;}
        memcpy(&task,value,sizeof(task));
        task_update_effects++;
        if(late_race==LATE_READBACK_DEATH)late_detach();
        if(fail_idle_after_effect) {errno=EIO;return -1;}
        return 0;
    }
    assert(map==COMMANDS && *(const u32 *)key==7);memcpy(&command,value,sizeof(command));return 0;
}
int bpf_map_delete_elem(int map,const void *key) {
    deletes++;
    if(map==TASKS) {
        if(*(const int *)key==12) {child_absent=true;return 0;}
        assert(*(const int *)key==9);
        if(late_race==LATE_DELETE_MISSING) {late_detach();errno=late_errno;return late_rc;}
        task_absent=true;task_delete_effects++;
        if(detach_after_delete)late_detach();
        if(fail_task_delete_after_effect) {errno=EIO;return -1;}
        return 0;
    }
    assert(map==CALLS);row_absent=true;
    if(fail_row_delete_after_effect) {errno=EIO;return -1;}
    return 0;
}
static void reset(struct ap_session *s,int returned) {
    late_race=LATE_NONE;late_boundary=late_hup=false;allow_hup=true;
    detach_after_delete=false;
    late_rc=-ENOENT;late_errno=ENOENT;missing_rc=-1;missing_errno=ESRCH;
    poll_errno_clobber=late_lookup_failure=late_poll_bits=0;
    late_polls=late_lookups=task_update_effects=task_delete_effects=0;
    memset(s,0,sizeof(*s));s->ready=true;s->incarnation=3;s->stream_copy_ring=&empty_test_ring;
    /* The status reader now authenticates the same retained observer. Model
     * its actual roles through the existing full metadata query controls. */
    s->fdget_program=201;s->fdget_link=202;
    s->copy_program[0]=203;s->copy_link[0]=204;
    s->copy_program[1]=205;s->copy_link[1]=206;
    s->selection_program[0]=207;s->selection_link[0]=208;
    s->selection_program[1]=209;s->selection_link[1]=210;
    s->selection_program[2]=209;s->selection_link[2]=212;
    s->selection_program[3]=209;s->selection_link[3]=214;
    s->selection_program[4]=209;s->selection_link[4]=216;
    s->selection_program[5]=209;s->selection_link[5]=218;
    s->selection_program[6]=209;s->selection_link[6]=220;
    s->selection_program[7]=209;s->selection_link[7]=222;
    s->selection_program[8]=209;s->selection_link[8]=224;
    s->links_count=6;
    for(unsigned i=0;i<6;i++) {
        unsigned program=i<4?321+i*2:325+(i-4)*2;
        unsigned link=i<4?322+i*2:330+(i-4)*2;
        /* Distinct modeled handle populations: the new epoll selection
         * links must not be mistaken for stream-copy metadata. These numbers
         * are fixture labels; the actual FD and identifier caps stay128. */
        for(unsigned role=0;role<9;role++) {
            assert((int)program!=s->selection_program[role] && (int)program!=s->selection_link[role]);
            assert((int)link!=s->selection_program[role] && (int)link!=s->selection_link[role]);
        }
        metadata_stream_links[i]=(struct bpf_link){i,(int)link};
        s->links[i]=&metadata_stream_links[i];s->stream_link[i]=i;s->stream_program[i]=(int)program;
        s->link_identity[i]=(struct ap_link_identity){i<2?BPF_LINK_TYPE_KPROBE_MULTI:BPF_LINK_TYPE_PERF_EVENT,
            link,program,BPF_PROG_TYPE_KPROBE};
    }
    info_bad=info_fail_at=0;
    s->tasks=TASKS;s->commands=COMMANDS;s->status=STATUS;
    atomic_flag_clear(&s->command_busy);
    task=(struct ap_task_command){.provider=3,.command=7,.operation=AP_NATIVE_BIRTH,
        .expected_object=17,.generation_before=47,.generation_after=23,.expected_level=435};
    command=(struct ap_command_result){.command=7,.operation=AP_NATIVE_BIRTH,
        .task=(5001ULL<<32)|5001,.start_boottime=29,.identity={.provider=3},
        .returned=returned,.phase=AP_COMMAND_DONE};
    row=(struct ap_fd_call){.command=7,.operation=AP_NATIVE_BIRTH};
    row.birth=(struct ap_native_birth){.command=7,.call=17,.owner_mm=23,.provider=3,
        .creator_task=command.task,.creator_start=29,.creator_table=47,
        .child_task=(5012ULL<<32)|5012,.child_start=31,.child_table=53,
        .parent_task=command.task,.parent_start=29,.copy_begin=59,.copy_end=61,
        .ready=returned>0,.exit_signal=17,.requested_exit_signal=17,.pidfd_fd=-1};
    s->pending[7].state=AP_SLOT_ACTIVE;s->pending[7].submitted=task;
    task_absent=row_absent=dead=child_absent=false;
    task_reads=row_reads=updates=deletes=idle_writes=command_reads=0;
    die_at_task_read=die_after_row_read=die_at_idle_write=false;
    fail_task_delete_after_effect=fail_idle_after_effect=fail_row_delete_after_effect=false;
    torn_command=torn_row=false;poll_failure=0;
    if(returned>0) {
        // Use the production observation/admission before queued Collect.
        struct ap_native_birth admitted;
        assert(ap_admit_native_birth_child(s,12,7,&admitted)==0);
        assert(child_absent && s->pending[7].birth_child_admitted);
        task_reads=row_reads=updates=deletes=idle_writes=command_reads=0;
    }
}
static unsigned checks;
#define CHECK(value) do {assert(value);checks++;} while(0)
static int inherited_main(void) {
    struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
    for(unsigned failed=0;failed<2;failed++)for(unsigned race=0;race<5;race++) {
        reset(&s,failed?-EAGAIN:5012);
        // Same exclusive queued Collect, original retained pin and command.
        const u64 queued_command=s.pending[7].submitted.command;
        const int held_creator=9;
        if(race==1)dead=true;
        if(race==2)dead=task_absent=true;
        if(race==3)die_at_task_read=true;
        if(race==4)die_after_row_read=true;
        struct ap_command_result original=command;struct ap_fd_call original_row=row;
        CHECK(ap_collect_native_birth(&s,held_creator,queued_command,&raw,&birth)==0);
        CHECK(!memcmp(&raw,&original,sizeof(raw)) && !memcmp(&row,&original_row,sizeof(row)));
        CHECK(raw.phase==AP_COMMAND_DONE && raw.returned==(failed?-EAGAIN:5012));
        CHECK(s.pending[7].state==AP_SLOT_COLLECTED && s.pending[7].birth_collected);
        CHECK(idle_writes==(race==0) && !row_absent);
        CHECK(ap_ack_command(&s,&raw)==0);
        CHECK(s.pending[7].state==AP_SLOT_FREE && row_absent && command.command==0);
    }
    for(unsigned bad=0;bad<13;bad++) {
        reset(&s,5012);dead=task_absent=true;
        if(bad==0)dead=false;
        if(bad==1)command.phase=AP_COMMAND_RUNNING;
        if(bad==2)command.phase=AP_COMMAND_READY;
        if(bad==3)row.birth.creator_start++;
        if(bad==4)row.birth.provider++;
        if(bad==5)row.birth.owner_mm++;
        if(bad==6)torn_command=true;
        if(bad==7)torn_row=true;
        if(bad==8)poll_failure=EINTR;
        if(bad==9)row.birth.child_start++;
        if(bad==10)row.birth.call++;
        if(bad==11)row.birth.creator_table++;
        if(bad==12)command.identity.provider++;
        CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        CHECK(s.pending[7].state==AP_SLOT_ACTIVE && updates==0 && deletes==0 && !row_absent);
        CHECK(command.command==7);
    }
    for(unsigned failure=0;failure<3;failure++) {
        reset(&s,5012);
        if(failure==0)die_at_idle_write=true;
        if(failure==1) {dead=true;fail_task_delete_after_effect=true;}
        if(failure==2)fail_idle_after_effect=true;
        CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && s.pending[7].birth_collected);
        CHECK(command.command==7 && !row_absent && raw.returned==5012);
        unsigned prior_updates=updates,prior_deletes=deletes;
        CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        CHECK(updates==prior_updates && deletes==prior_deletes && s.pending[7].state==AP_SLOT_QUARANTINED);
    }
    reset(&s,-ENOMEM);dead=task_absent=true;
    CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==0);
    fail_row_delete_after_effect=true;
    CHECK(ap_ack_command(&s,&raw)==-1 && s.pending[7].state==AP_SLOT_QUARANTINED);
    CHECK(raw.returned==-ENOMEM && command.command==7);
    for(unsigned absent=0;absent<2;absent++) {
        reset(&s,-EAGAIN);dead=true;task_absent=absent;row_absent=true;
        command=(struct ap_command_result){.command=7,.operation=AP_NATIVE_BIRTH,.phase=AP_COMMAND_READY};
        CHECK(ap_cancel_uninvoked_birth(&s,9,7)==0);
        CHECK(s.pending[7].state==AP_SLOT_FREE && task_absent && idle_writes==0 && command.command==0);
    }
    reset(&s,-EAGAIN);dead=task_absent=true;
    CHECK(ap_cancel_uninvoked_birth(&s,9,7)==-1 && errno==ESTALE);
    CHECK(s.pending[7].state==AP_SLOT_ACTIVE && updates==0 && deletes==0);
    assert(checks==133);
    printf("native birth production driver cleanup: %u checks\n",checks);
    return 0;
}

static unsigned late_checks;
#define LATE_CHECK(value) do {assert(value);late_checks++;} while(0)
static void late_reset(struct ap_session *s,int returned,enum late_race race) {
    reset(s,returned);late_race=race;missing_rc=-ENOENT;missing_errno=ENOENT;
    if(race==LATE_DELETE_MISSING)dead=true; /* positive zombie lookup before removal */
    poll_errno_clobber=EBUSY;
}
static void late_success(enum late_race race,int returned) {
    struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
    late_reset(&s,returned,race);
    const struct ap_command_result original=command;
    const struct ap_fd_call original_row=row;
    LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==0);
    LATE_CHECK(!memcmp(&raw,&original,sizeof(raw)) && !memcmp(&row,&original_row,sizeof(row)));
    LATE_CHECK(raw.phase==AP_COMMAND_DONE && raw.returned==returned);
    LATE_CHECK(s.pending[7].state==AP_SLOT_COLLECTED && s.pending[7].disarm.detached_verified);
    enum ap_task_disarm_phase phase=race==LATE_UPDATE_MISSING?AP_DISARM_IDLE_UPDATE_RETURNED:
        race==LATE_DELETE_MISSING?AP_DISARM_DEAD_DELETE_RETURNED:AP_DISARM_IDLE_READBACK_RETURNED;
    LATE_CHECK(s.pending[7].disarm.phase==phase);
    LATE_CHECK(s.pending[7].disarm.mutation_rc==(race==LATE_READBACK_DEATH?0:-ENOENT));
    LATE_CHECK(s.pending[7].disarm.mutation_errno==(race==LATE_READBACK_DEATH?0:ENOENT));
    LATE_CHECK(s.pending[7].disarm.readback_rc==(race==LATE_READBACK_DEATH?-ENOENT:0) &&
               s.pending[7].disarm.readback_errno==(race==LATE_READBACK_DEATH?ENOENT:0));
    LATE_CHECK(late_polls==1 && late_lookups==1);
    LATE_CHECK(idle_writes==(race!=LATE_DELETE_MISSING) && deletes==(race==LATE_DELETE_MISSING));
    LATE_CHECK(task_update_effects==(race==LATE_READBACK_DEATH) && task_delete_effects==0);
    LATE_CHECK(!row_absent && command.command==7);
    unsigned saved_updates=updates,saved_deletes=deletes;
    struct ap_command_result duplicate;
    LATE_CHECK(ap_collect_native_birth(&s,9,7,&duplicate,&birth)==-1);
    LATE_CHECK(updates==saved_updates && deletes==saved_deletes && s.pending[7].state==AP_SLOT_COLLECTED);
    LATE_CHECK(ap_ack_command(&s,&raw)==0);
    LATE_CHECK(s.pending[7].state==AP_SLOT_FREE && row_absent && command.command==0);
}
/* Existing production command/selection/Collect/ACK owner, with the same
 * lower map/poll boundary model. This is not Native admission evidence. */
static void scalar_driver_reset(struct ap_session *s,int returned) {
    reset(s,-EAGAIN);
    task=(struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=17,.generation_before=0x10000000004ULL,.generation_after=23,
        .expected_level=4,.original_count=0x10000000020ULL};
    command.operation=AP_ORIGINAL_READ;command.returned=returned;command.original_count=task.original_count;
    row=(struct ap_fd_call){.command=7,.operation=AP_ORIGINAL_READ};
    row.original=(struct ap_original_result){.selection={.command=7,.call=17,.owner_mm=23,
        .provider=3,.task=command.task,.task_start=29,.table=47,.file=returned==-9?0:53,
        .user_address=task.generation_before,.fdput_flags=returned==-9?0:1,.ready=1,
        .requested_fd=4,.original_count=task.original_count},.returned=returned,.complete=1};
    s->pending[7].submitted=task;
    /* This fixture's regular-file/early-error shape did not enter a socket
     * protocol. The actual syscall exit still emits its explicit empty commit. */
    struct ap_stream_copy_record commit={.provider=3,.command=7,.call=17,
        .task=command.task,.task_start=29,.sequence=1,.offset=(u64)(s64)returned,
        .length=sizeof(struct ap_stream_copy_summary),.kind=AP_STREAM_COPY_COMMIT};
    assert(stream_copy_record(s,&commit,sizeof(commit))==0);
}
static void scalar_original_driver_controls(void) {
    unsigned scalar_checks=0;
#define RD_CHECK(v) do {assert(v);scalar_checks++;} while(0)
    const int returns[]={0,7,-9,-512};
    for(unsigned i=0;i<4;i++) {
        struct ap_session s;scalar_driver_reset(&s,returns[i]);
        struct ap_original_selection selected;struct ap_command_result raw;struct ap_original_result observed;
        RD_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
        RD_CHECK(selected.original_count==task.original_count && selected.user_address==task.generation_before);
        struct ap_command_result old=command;struct ap_fd_call oldrow=row;
        RD_CHECK(ap_collect_original_connect(&s,9,7,&raw,&observed)==0);
        RD_CHECK(!memcmp(&raw,&old,sizeof(raw)) && !memcmp(&row,&oldrow,sizeof(row)));
        RD_CHECK(s.pending[7].state==AP_SLOT_COLLECTED && s.pending[7].original_collected && idle_writes==1);
        struct ap_stream_copy_manifest copy;
        assert(ap_original_read_copy_manifest(&s,7,&copy)==0 && copy.present==0);
        assert(copy.returned==returns[i] && copy.call==s.pending[7].submitted.expected_object);
        RD_CHECK(ap_ack_command(&s,&raw)==0);
        RD_CHECK(s.pending[7].state==AP_SLOT_FREE && row_absent && !command.command);
    }
    for(unsigned bad=0;bad<7;bad++) {
        struct ap_session s;scalar_driver_reset(&s,7);
        struct ap_original_selection selected;struct ap_command_result raw;struct ap_original_result observed;
        if(bad==0)row.original.selection.ready=0;
        if(bad==1)row.original.selection.original_count^=1ULL<<32;
        if(bad==2)row.original.selection.user_address^=1ULL<<32;
        if(bad==3)command.original_count++;
        if(bad<4)RD_CHECK(ap_read_original_selection(&s,9,7,&selected)==-1);
        else {
            RD_CHECK(ap_read_original_selection(&s,9,7,&selected)==0);
            if(bad==4)row.original.selection.table++;
            if(bad==5)row.original.returned=8;
            if(bad==6)command.original_count++;
            RD_CHECK(ap_collect_original_connect(&s,9,7,&raw,&observed)==-1);
        }
        RD_CHECK(s.pending[7].state==AP_SLOT_ACTIVE && !row_absent && !updates && !deletes);
    }
    struct ap_session s;scalar_driver_reset(&s,7);u64 out=123;
    RD_CHECK(ap_prepare_original_read(NULL,9,17,23,4,0,0,&out)==-1 && out==123);
    RD_CHECK(ap_prepare_original_read(&s,9,0,23,4,0,0,&out)==-1 && out==123);
    RD_CHECK(ap_prepare_original_read(&s,9,17,23,4,0,0,NULL)==-1 && out==123);
    assert(scalar_checks==48);
    printf("scalar Read production driver selection/Collect/ACK: %u checks\n",scalar_checks);
#undef RD_CHECK
}

/* The same physical command ACK must not release a Read while the service
 * has not retained its copy manifest and all fragments. These controls execute
 * the production Collect and ACK, including quarantine on an invalid ACK;
 * they never repair the rejected command and then call that a successful ACK. */
static void scalar_copy_ack_controls(void) {
    struct ap_session s;struct ap_original_selection selected;
    struct ap_command_result raw;struct ap_original_result observed;
    struct ap_stream_copy_manifest manifest;struct ap_stream_copy_record record;
    scalar_driver_reset(&s,7);
    assert(ap_read_original_selection(&s,9,7,&selected)==0);
    s.pending[7].stream_copy=(struct ap_stream_copy_owned){0};
    assert(ap_collect_original_connect(&s,9,7,&raw,&observed)==-1 && errno==EAGAIN);
    assert(s.pending[7].state==AP_SLOT_ACTIVE && !s.pending[7].original_collected);
    assert(!updates && !deletes && !row_absent);
    assert(command.command==7 && task.command==7);

    for(unsigned missing=0;missing<2;missing++) {
        scalar_driver_reset(&s,7);
        struct ap_pending_command *p=&s.pending[7];
        if(missing==1) {
            /* One actual callback fragment followed by its actual-exit commit,
             * delivered into the retained command before any physical ACK. */
            p->stream_copy=(struct ap_stream_copy_owned){0};
            record=(struct ap_stream_copy_record){.provider=3,.command=7,.call=17,
                .task=command.task,.task_start=command.start_boottime,.sequence=1,.attempt=1,
                .length=7,.kind=AP_STREAM_COPY_DATA};
            memset(record.bytes,'Z',7);
            assert(stream_copy_record(&s,&record,sizeof(record))==0);
            const struct ap_stream_copy_unit unit={.file=23,.order=1,.requested=7,.copied=7,
                .transport=AP_STREAM_COPY_TCP,.disposition=AP_STREAM_COPY_CONSUME};
            record=(struct ap_stream_copy_record){.provider=3,.command=7,.call=17,
                .task=command.task,.task_start=command.start_boottime,.sequence=2,.attempt=1,
                .length=sizeof(unit),.kind=AP_STREAM_COPY_UNIT};
            memcpy(record.bytes,&unit,sizeof(unit));
            assert(stream_copy_record(&s,&record,sizeof(record))==0);
            const struct ap_stream_copy_summary summary={.version=AP_STREAM_COPY_VERSION,.initial_count=7,
                .attempts=1,.records=2,.copied=7,.protocol_returned=7,.protocol_complete=1};
            record=(struct ap_stream_copy_record){.provider=3,.command=7,.call=17,
                .task=command.task,.task_start=command.start_boottime,.sequence=3,.attempt=1,
                .offset=7,.length=sizeof(summary),.kind=AP_STREAM_COPY_COMMIT};
            memcpy(record.bytes,&summary,sizeof(summary));
            assert(stream_copy_record(&s,&record,sizeof(record))==0);
        }
        assert(ap_read_original_selection(&s,9,7,&selected)==0);
        assert(ap_collect_original_connect(&s,9,7,&raw,&observed)==0);
        if(missing==1) {
            assert(ap_original_read_copy_manifest(&s,7,&manifest)==0);
            assert(manifest.present==1 && manifest.summary.records==2);
        }
        const unsigned prior_updates=updates,prior_deletes=deletes;
        const struct ap_command_result prior=command;
        const struct ap_fd_call prior_row=row;
        assert(ap_ack_command(&s,&raw)==-1 && errno==EPROTO);
        assert(p->state==AP_SLOT_QUARANTINED && !row_absent);
        assert(updates==prior_updates && deletes==prior_deletes);
        assert(!memcmp(&command,&prior,sizeof(command)) && !memcmp(&row,&prior_row,sizeof(row)));
        assert(!p->stream_copy.manifest_read || p->stream_copy.delivered!=p->stream_copy.count);
        assert(ap_ack_command(&s,&raw)==-1 && errno==ESTALE);
        assert(ap_original_read_copy_record(&s,7,0,&record)==-1 && errno==EINVAL);
        free(p->stream_copy.records);p->stream_copy.records=NULL;
    }
    puts("Read copy physical ACK: missing commit leaves ACTIVE; missing manifest or fragment retains quarantined command and native receipt");
}

/* R1/R2: genuine production prepare -> submit -> READY -> cleanup, using
 * this translation unit's existing lower map/pidfd boundaries. Neither
 * the reservation nor submit is substituted, and no producer/injection runs. */
static unsigned read_cleanup_checks;
#define READY_READ_CHECK(v) do {assert(v);read_cleanup_checks++;} while(0)
static u64 prepare_ready_read(struct ap_session *s,u64 count) {
    reset(s,-EAGAIN);
    /* Existing registered idle task and empty shared slot are the actual
     * submit preconditions. Prepare writes the only READY row below. */
    s->next_command=6;
    memset(&s->pending[7],0,sizeof(s->pending[7]));
    task=(struct ap_task_command){.provider=3};
    command=(struct ap_command_result){0};row=(struct ap_fd_call){0};row_absent=true;
    u64 ticket=0;
    READY_READ_CHECK(ap_prepare_original_read(s,9,17,23,4,0x10000000004ULL,count,&ticket)==0);
    struct ap_task_command expected={.provider=3,.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=17,.generation_before=0x10000000004ULL,.generation_after=23,
        .expected_level=4,.original_count=count};
    READY_READ_CHECK(ticket==7 && !memcmp(&task,&expected,sizeof(task)) &&
        !memcmp(&s->pending[7].submitted,&expected,sizeof(expected)));
    const struct ap_command_result ready={.command=7,.operation=AP_ORIGINAL_READ,
        .phase=AP_COMMAND_READY,.original_count=count};
    READY_READ_CHECK(!memcmp(&command,&ready,sizeof(command)));
    READY_READ_CHECK(s->pending[7].state==AP_SLOT_ACTIVE &&
        !s->pending[7].original_selected && !s->pending[7].original_collected);
    READY_READ_CHECK(updates==2 && !deletes && !row_reads && row_absent);
    return ticket;
}
static void original_read_ready_cleanup_controls(void) {
    read_cleanup_checks=0;
    const u64 counts[]={0,1,1ULL<<32,0x10000000020ULL};
    const struct ap_command_result empty_result={0};
    const struct ap_pending_command empty_pending={0};
    const struct ap_original_result empty_original={0};
    const struct ap_task_command idle={.provider=3};
    for(unsigned i=0;i<4;i++) {
        struct ap_session s;u64 ticket=prepare_ready_read(&s,counts[i]);
        const struct ap_command_result actual_ready=command;
        const struct ap_task_command actual_submitted=s.pending[7].submitted;
        /* Separate known-uninvoked authority is a supplied control premise:
         * no injection or producer call occurs after actual prepare. READY
         * itself does not grant that authority to a production caller. */
        READY_READ_CHECK(ap_cancel_uninvoked_original(&s,9,ticket)==0);
        READY_READ_CHECK(!memcmp(&s.pending[7],&empty_pending,sizeof(empty_pending)) &&
            !memcmp(&command,&empty_result,sizeof(command)));
        READY_READ_CHECK(!memcmp(&task,&idle,sizeof(task)) && updates==4 && !deletes);
        READY_READ_CHECK(actual_ready.phase==AP_COMMAND_READY && actual_ready.original_count==counts[i] &&
            actual_submitted.original_count==counts[i]);
        READY_READ_CHECK(row_absent && !row_reads && !row.original.selection.ready && !row.original.complete);
    }
    for(unsigned i=0;i<4;i++)for(unsigned absent=0;absent<2;absent++) {
        struct ap_session s;u64 ticket=prepare_ready_read(&s,counts[i]);
        const struct ap_command_result actual_ready=command;
        /* Model the original retained PIDFD_THREAD's positive death, not a
         * numeric PID disappearance. The exact pidfd remains9 in fake poll. */
        dead=true;task_absent=absent;
        struct ap_original_terminal terminal;memset(&terminal,0xa5,sizeof(terminal));
        READY_READ_CHECK(ap_retire_dead_original(&s,9,ticket,&terminal)==0);
        READY_READ_CHECK(!memcmp(&terminal.command,&actual_ready,sizeof(actual_ready)) &&
            terminal.command.phase==AP_COMMAND_READY && terminal.command.original_count==counts[i]);
        READY_READ_CHECK(terminal.call==17 && terminal.task_absent==1 && !terminal.fd_call_present &&
            !memcmp(&terminal.original,&empty_original,sizeof(empty_original)));
        READY_READ_CHECK(!memcmp(&s.pending[7],&empty_pending,sizeof(empty_pending)) &&
            !memcmp(&command,&empty_result,sizeof(command)));
        READY_READ_CHECK(task_absent && updates==3 && deletes==(absent?0U:1U));
        READY_READ_CHECK(row_absent && !row_reads && !row.original.selection.ready && !row.original.complete);
    }
    /* Every corruption starts from the actual count-bearing submit output.
     * Includes zero vs1<<32 in both directions, never a low32-only check. */
    for(unsigned i=0;i<4;i++)for(unsigned route=0;route<3;route++) {
        struct ap_session s;u64 ticket=prepare_ready_read(&s,counts[i]);
        command.original_count^=1ULL<<32;
        const struct ap_command_result wrong_ready=command;
        const struct ap_task_command before_task=task;
        const struct ap_pending_command before_pending=s.pending[7];
        if(route) {dead=true;task_absent=route==2;}
        struct ap_original_terminal terminal={0};errno=0;
        int rc=route?ap_retire_dead_original(&s,9,ticket,&terminal):
            ap_cancel_uninvoked_original(&s,9,ticket);
        READY_READ_CHECK(rc==-1 && errno==(route?EPROTO:ESTALE));
        READY_READ_CHECK(updates==2 && !deletes && !memcmp(&command,&wrong_ready,sizeof(command)) &&
            !memcmp(&task,&before_task,sizeof(task)) &&
            !memcmp(&s.pending[7],&before_pending,sizeof(before_pending)));
        READY_READ_CHECK(row_absent && !row_reads && s.pending[7].state==AP_SLOT_ACTIVE);
        READY_READ_CHECK(!route || (!memcmp(&terminal.command,&wrong_ready,sizeof(wrong_ready)) &&
            terminal.command.phase==AP_COMMAND_READY && !terminal.fd_call_present && !terminal.task_absent));
    }
    /* The count repair must not turn READY/absence into death authority. */
    for(unsigned denial=0;denial<3;denial++) {
        struct ap_session s;u64 ticket=prepare_ready_read(&s,counts[3]);
        struct ap_original_terminal terminal={0};
        const struct ap_command_result actual_ready=command;
        const struct ap_task_command before_task=task;
        const struct ap_pending_command before_pending=s.pending[7];
        if(denial)dead=true;
        if(denial==2)poll_failure=EINTR;
        errno=0;
        READY_READ_CHECK(ap_retire_dead_original(&s,denial==1?10:9,ticket,&terminal)==-1 &&
            errno==(denial==2?EINTR:EAGAIN));
        READY_READ_CHECK(!memcmp(&command,&actual_ready,sizeof(command)) &&
            !memcmp(&task,&before_task,sizeof(task)) &&
            !memcmp(&s.pending[7],&before_pending,sizeof(before_pending)));
        READY_READ_CHECK(updates==2 && !deletes && !task_absent && row_absent && !row_reads);
    }
    /* Reuse existing unknown-outcome controls: no repair promotes mutation
     * errors to cancellation/death success or frees an uncertain reservation. */
    for(unsigned retirement=0;retirement<2;retirement++) {
        struct ap_session s;u64 ticket=prepare_ready_read(&s,counts[3]);
        const struct ap_command_result actual_ready=command;
        struct ap_original_terminal terminal={0};
        if(retirement) {dead=true;fail_task_delete_after_effect=true;}
        else fail_idle_after_effect=true;
        errno=0;
        int rc=retirement?ap_retire_dead_original(&s,9,ticket,&terminal):
            ap_cancel_uninvoked_original(&s,9,ticket);
        READY_READ_CHECK(rc==-1 && errno==EIO);
        READY_READ_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED &&
            s.pending[7].submitted.original_count==counts[3] &&
            !memcmp(&command,&actual_ready,sizeof(command)));
        READY_READ_CHECK(row_absent && !row_reads &&
            (retirement?(task_absent && updates==2 && deletes==1):
                (!task_absent && updates==3 && !deletes && !memcmp(&task,&idle,sizeof(task)))));
        READY_READ_CHECK(!retirement || (!memcmp(&terminal.command,&actual_ready,sizeof(actual_ready)) &&
            terminal.command.phase==AP_COMMAND_READY && !terminal.fd_call_present));
    }
    assert(read_cleanup_checks==278);
    printf("scalar Read actual prepare/READY cleanup: %u checks\n",read_cleanup_checks);
}
#undef READY_READ_CHECK

/* Actual shared prepare/submit/Collect/ACK/cancel/death paths. Only the same
 * lower map/pidfd boundary model above supplies kernel-shaped rows here. */
static unsigned epoll_driver_checks;
#define EC_CHECK(v) do {assert(v);epoll_driver_checks++;} while(0)
static void epoll_ready(struct ap_session *s,int op,u64 pointer) {
    reset(s,-EAGAIN);s->next_command=6;
    memset(&s->pending[7],0,sizeof(s->pending[7]));
    task=(struct ap_task_command){.provider=3};command=(struct ap_command_result){0};
    row=(struct ap_fd_call){0};row_absent=true;u64 ticket=0;
    EC_CHECK(ap_prepare_epoll_ctl_copy(s,9,17,23,op,pointer,&ticket)==0 && ticket==7);
    const struct ap_task_command expected={.provider=3,.command=7,.operation=AP_EPOLL_CTL_COPY,
        .expected_object=17,.generation_before=pointer,.generation_after=23,
        .expected_level=op,.expected_option=233};
    const struct ap_command_result reserved={.command=7,.operation=AP_EPOLL_CTL_COPY,.phase=AP_COMMAND_READY};
    EC_CHECK(!memcmp(&task,&expected,sizeof(task)) && !memcmp(&command,&reserved,sizeof(command)));
    EC_CHECK(updates==2 && !deletes && row_absent && !s->pending[7].epoll_collected);
}
static void epoll_producer_row(int op,int returned) {
    command.task=(5001ULL<<32)|5001;command.start_boottime=29;
    command.identity.provider=3;command.returned=returned;command.phase=AP_COMMAND_DONE;
    row=(struct ap_fd_call){.command=7,.operation=AP_EPOLL_CTL_COPY};row_absent=false;
    row.epoll_copy=(struct ap_epoll_ctl_copy){.command=7,.call=17,.owner_mm=23,.provider=3,
        .task=command.task,.task_start=29,.table=47,.user_address=task.generation_before,
        .entered=1,.ctl_entered=returned==-9,.ctl_returned=returned==-9,.complete=1,
        .image_wakeup_policy=AP_EPOLL_IMAGE_PM_SLEEP_DISABLED,.op=op,.returned=returned};
    if(returned==-9 && op!=2)for(unsigned n=0;n<12;n++)row.epoll_copy.event[n]=(u8)(0xa0+n);
}
static void epoll_ctl_copy_driver_controls(void) {
    epoll_driver_checks=0;const int ops[]={1,2,3,-1,0,0x7fffffff};
    for(unsigned i=0;i<sizeof(ops)/sizeof(ops[0]);i++)for(unsigned fault=0;fault<2;fault++) {
        if(fault && ops[i]==2)continue; /* DEL has no native uaccess/EFAULT branch. */
        struct ap_session s;epoll_ready(&s,ops[i],0x10000000004ULL);
        struct ap_command_result raw;struct ap_epoll_ctl_copy copied;
        EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==-1 && errno==ENODATA);
        EC_CHECK(updates==2 && !deletes && s.pending[7].state==AP_SLOT_ACTIVE);
        epoll_producer_row(ops[i],fault?-14:-9);
        const struct ap_fd_call actual=row;const struct ap_command_result actual_command=command;
        EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==0);
        EC_CHECK(!memcmp(&raw,&actual_command,sizeof(raw)) && !memcmp(&copied,&actual.epoll_copy,sizeof(copied)));
        EC_CHECK(s.pending[7].epoll_collected && !s.pending[7].original_selected && !s.pending[7].original_collected);
        EC_CHECK(updates==3 && !deletes && !row_absent && !memcmp(&row,&actual,sizeof(row)));
        EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==-1 && updates==3 && !deletes);
        EC_CHECK(ap_ack_command(&s,&actual_command)==0);
        EC_CHECK(s.pending[7].state==AP_SLOT_FREE && row_absent && !command.command && updates==4 && deletes==1);
    }
    for(unsigned bad=0;bad<13;bad++) {
        struct ap_session s;epoll_ready(&s,1,0x10000000004ULL);epoll_producer_row(1,-9);
        if(bad==0)row.epoll_copy.entered=0;
        if(bad==1)row.epoll_copy.ctl_entered=0;
        if(bad==2)row.epoll_copy.ctl_returned=0;
        if(bad==3)row.epoll_copy.user_address^=1ULL<<32;
        if(bad==4)row.epoll_copy.owner_mm++;
        if(bad==5)row.epoll_copy.call++;
        if(bad==6)row.epoll_copy.image_wakeup_policy=0;
        if(bad==7)row.epoll_copy.returned=-14;
        if(bad==8)row.epoll_copy.reserved=1;
        if(bad==9)row.epoll_copy.problem=AP_FD_MISSING;
        if(bad==10)row.epoll_copy.complete=0;
        if(bad==11)command.original_count=1ULL<<32;
        if(bad==12)torn_row=true;
        struct ap_command_result raw;struct ap_epoll_ctl_copy copied;
        EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==-1 && errno==ENODATA);
        EC_CHECK(updates==2 && !deletes && !row_absent && s.pending[7].state==AP_SLOT_ACTIVE);
    }
    for(unsigned route=0;route<3;route++) {
        struct ap_session s;epoll_ready(&s,2,~0ULL);
        const struct ap_command_result reserved=command;
        if(!route) {
            EC_CHECK(ap_cancel_uninvoked_epoll_ctl_copy(&s,9,7)==0);
            EC_CHECK(!task_absent && updates==4 && !deletes);
        } else {
            dead=true;task_absent=route==2;struct ap_epoll_ctl_terminal terminal;
            EC_CHECK(ap_retire_dead_epoll_ctl_copy(&s,9,7,&terminal)==0);
            EC_CHECK(!memcmp(&terminal.command,&reserved,sizeof(reserved)) && terminal.task_absent==1 &&
                !terminal.fd_call_present && !terminal.copy.entered && !terminal.copy.complete);
            EC_CHECK(updates==3 && deletes==(route==1) && task_absent);
        }
        EC_CHECK(row_absent && !row_reads && !command.command && s.pending[7].state==AP_SLOT_FREE);
    }
    for(unsigned denial=0;denial<4;denial++) {
        struct ap_session s;epoll_ready(&s,1,0x10000000004ULL);
        if(denial)dead=true;
        if(denial==2)poll_failure=EINTR;
        if(denial==3)command.original_count=1ULL<<32;
        struct ap_epoll_ctl_terminal terminal={0};
        EC_CHECK(ap_retire_dead_epoll_ctl_copy(&s,denial==1?10:9,7,&terminal)==-1);
        EC_CHECK(updates==2 && !deletes && !task_absent && row_absent && s.pending[7].state==AP_SLOT_ACTIVE);
    }
    for(unsigned phase=AP_COMMAND_DONE;phase<=AP_COMMAND_RUNNING;phase++) {
        if(phase==AP_COMMAND_READY)continue; /* actual READY case is exercised above */
        struct ap_session s;epoll_ready(&s,1,0x10000000004ULL);epoll_producer_row(1,-9);
        command.phase=phase;if(phase==AP_COMMAND_RUNNING) {
            command.returned=0;row.epoll_copy.returned=0;row.epoll_copy.complete=0;
            row.epoll_copy.ctl_returned=0;row.epoll_copy.problem=AP_FD_MISSING;
        }
        const struct ap_fd_call actual=row;const struct ap_command_result actual_command=command;
        dead=true;struct ap_epoll_ctl_terminal terminal;
        EC_CHECK(ap_retire_dead_epoll_ctl_copy(&s,9,7,&terminal)==0);
        EC_CHECK(!memcmp(&terminal.command,&actual_command,sizeof(actual_command)) &&
            !memcmp(&terminal.copy,&actual.epoll_copy,sizeof(terminal.copy)) && terminal.fd_call_present && terminal.task_absent);
        EC_CHECK(row_absent && task_absent && s.pending[7].state==AP_SLOT_FREE);
    }
    for(unsigned bad=0;bad<3;bad++) {
        struct ap_session s;epoll_ready(&s,1,0x10000000004ULL);epoll_producer_row(1,-9);
        struct ap_command_result raw;struct ap_epoll_ctl_copy copied;
        if(!bad) {
            fail_idle_after_effect=true;
            EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==-1 && errno==EIO);
        } else {
            EC_CHECK(ap_collect_epoll_ctl_copy(&s,9,7,&raw,&copied)==0);
            if(bad==1)row.epoll_copy.event[11]^=1;
            else fail_row_delete_after_effect=true;
            EC_CHECK(ap_ack_command(&s,&raw)==-1 && errno==(bad==1?ESTALE:EIO));
        }
        EC_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && command.command==7);
    }
    printf("epoll copy production command/receipt cleanup: %u checks (host boundary model)\n",epoll_driver_checks);
}
#undef EC_CHECK

int main(void) {
    epoll_ctl_copy_driver_controls();
    original_read_ready_cleanup_controls();
    scalar_original_driver_controls();
    scalar_copy_ack_controls();
    program_ownership_controls();
    observer_readiness_controls();
    assert(inherited_main()==0); /* all 133 inherited checks, unchanged */
    const int outcomes[]={5012,-EAGAIN,-ENOENT};
    for(unsigned i=0;i<3;i++)for(unsigned race=LATE_UPDATE_MISSING;race<=LATE_READBACK_DEATH;race++)
        late_success((enum late_race)race,outcomes[i]);
    for(unsigned bad=0;bad<12;bad++) {
        struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
        late_reset(&s,-ENOMEM,bad==11?LATE_DELETE_MISSING:LATE_UPDATE_MISSING);
        if(bad==0 || bad==11)allow_hup=false; /* POLLIN-only zombie is not detached */
        if(bad==1)late_rc=-EIO;
        if(bad==2)late_errno=EIO;
        if(bad>=3 && bad<=6)late_lookup_failure=(int)bad-2;
        if(bad==7)late_poll_bits=POLLHUP; /* require positive readable death too */
        if(bad==8)late_poll_bits=POLLIN|POLLHUP|POLLERR;
        if(bad==9)late_poll_bits=POLLIN|POLLHUP|POLLNVAL;
        if(bad==10) {missing_rc=-ESRCH;missing_errno=ESRCH;}
        LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        LATE_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && !s.pending[7].disarm.detached_verified);
        LATE_CHECK(command.command==7 && !row_absent && raw.returned==-ENOMEM);
        LATE_CHECK(task_update_effects==0 && task_delete_effects==0);
        unsigned saved_updates=updates,saved_deletes=deletes;
        LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        LATE_CHECK(updates==saved_updates && deletes==saved_deletes);
        LATE_CHECK(ap_ack_command(&s,&command)==-1 && s.pending[7].state==AP_SLOT_QUARANTINED);
    }
    for(unsigned bad=0;bad<4;bad++) {
        struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
        late_reset(&s,5012,LATE_READBACK_DEATH);
        if(bad==0)fail_idle_after_effect=true; /* EIO after real modeled write, even with HUP */
        if(bad==1)allow_hup=false;
        if(bad==2) {missing_rc=-EIO;missing_errno=EIO;}
        if(bad==3)late_lookup_failure=2;
        LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        LATE_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && !s.pending[7].disarm.detached_verified);
        LATE_CHECK(task_update_effects==1 && idle_writes==1 && deletes==0);
        LATE_CHECK(s.pending[7].disarm.mutation_rc==(bad==0?-1:0));
        LATE_CHECK(command.command==7 && !row_absent && raw.returned==5012);
        LATE_CHECK(ap_ack_command(&s,&command)==-1 && s.pending[7].state==AP_SLOT_QUARANTINED);
    }
    {
        struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
        late_reset(&s,5012,LATE_NONE);dead=true;
        detach_after_delete=fail_task_delete_after_effect=true;
        LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==-1);
        LATE_CHECK(task_delete_effects==1 && late_hup && deletes==1);
        LATE_CHECK(s.pending[7].disarm.phase==AP_DISARM_DEAD_DELETE_RETURNED &&
                   s.pending[7].disarm.mutation_rc==-1 && s.pending[7].disarm.mutation_errno==EIO);
        LATE_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && !s.pending[7].disarm.detached_verified);
        LATE_CHECK(late_polls==0 && !row_absent && command.command==7);
        LATE_CHECK(ap_ack_command(&s,&command)==-1 && s.pending[7].state==AP_SLOT_QUARANTINED);
    }
    {
        struct ap_session s;
        late_reset(&s,-EAGAIN,LATE_UPDATE_MISSING);row_absent=true;
        command=(struct ap_command_result){.command=7,.operation=AP_NATIVE_BIRTH,.phase=AP_COMMAND_READY};
        LATE_CHECK(ap_cancel_uninvoked_birth(&s,9,7)==-1);
        LATE_CHECK(s.pending[7].state==AP_SLOT_QUARANTINED && !s.pending[7].disarm.detached_verified);
        LATE_CHECK(s.pending[7].disarm.mutation_rc==-ENOENT && task_update_effects==0);
        LATE_CHECK(late_polls==0 && command.command==7);
    }
    {
        struct ap_session s;struct ap_command_result raw;struct ap_native_birth birth;
        late_reset(&s,-EAGAIN,LATE_DELETE_MISSING);
        LATE_CHECK(ap_collect_native_birth(&s,9,7,&raw,&birth)==0);
        fail_row_delete_after_effect=true;
        LATE_CHECK(ap_ack_command(&s,&raw)==-1 && s.pending[7].state==AP_SLOT_QUARANTINED);
        LATE_CHECK(raw.returned==-EAGAIN && command.command==7);
    }
    assert(late_checks==265);
    printf("native birth exact late-race recovery: %u checks (plus %u inherited)\n",late_checks,checks);
    return 0;
}
