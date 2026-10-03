/* SPDX-License-Identifier: MIT */
/* Execute the unmodified production TX callbacks and collector with controlled
 * kernel memory/map/ring boundaries. No BPF load or native execution is claimed. */
#define AP_FTRACE_PROVIDER 1
#define AP_GROUPED_PROVIDER 1
#include <assert.h>
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"
#include "grouped-probes.h"
#define CORE(x) (x)
struct pt_regs {u64 di,si,dx,sp,ip,bp,ax,orig_ax,cs,r10,r8,r9;};
struct rb_node {struct rb_node *rb_left,*rb_right;u64 __rb_parent_color;};
struct socket;
struct sock {
    struct {u8 skc_state;u16 skc_family;} __sk_common;
    u16 sk_protocol,sk_type;
    struct {s32 owned;} sk_lock;
    struct socket *sk_socket;
    struct {void *next,*prev;u32 qlen;} sk_write_queue;
    struct {struct rb_node *rb_node;} tcp_rtx_queue;
};
struct inet_sock {struct sock sk;u64 inet_flags;};
struct tcp_sock {struct sock sk;u64 inet_flags;u64 repair;u32 write_seq;};
struct file {u32 f_flags;};
struct socket {struct sock *sk;struct file *file;};
struct msghdr {u32 msg_flags;s32 msg_namelen;void *msg_name,*msg_control;u64 msg_controllen;struct {u64 count;} msg_iter;};
struct sk_buff {
    struct rb_node rbnode;void *next,*prev;
    u64 head,data;u32 tail,end,len,data_len;s32 users;u8 cb[48];
};
struct tcp_skb_cb {u32 seq,end_seq;unsigned short tcp_flags;};
struct skb_shared_info {struct {u64 netmem;u32 len,offset;} frags[17];};
/* Host structs model only the fields read by this producer. CO-RE offsets are
 * boundary premises; actual production shape/phase/byte checks remain intact. */
#define __builtin_preserve_field_info(field,which) ((which)==1?8:(which)==0? \
    (sizeof(field)==sizeof(struct rb_node)?offsetof(struct sk_buff,rbnode):offsetof(struct tcp_sock,repair)):0)
static struct ap_config config;
static struct ap_task_command submitted;
static struct ap_command_result receipt;
static struct ap_fd_call active;
static struct {void *files,*mm;} task;
static struct tcp_sock tcp;
static struct socket socket_value;
static struct file selected;
static struct msghdr message;
static struct sk_buff skb;
static u8 head[2048];
static u64 stack[64];
static int ap_config_map,stream_copy_records;
static struct ap_stream_copy_record emitted[8],reserved;
static unsigned emitted_count,published,checks;
static u64 problems;
static bool missing_call,missing_result,ring_full,read_failure,bad_image;
static unsigned timeout_reads;
static bool timeout_read_failure;
static int bad_window;
static struct ap_task_command *command(void) {return &submitted;}
static struct ap_command_result *result(u64 command_id) {
    return !missing_result && command_id==submitted.command?&receipt:NULL;
}
static struct ap_fd_call *fd_actor_call(struct ap_invocation_key *key) {
    key->task=13;key->start=17;return missing_call?NULL:&active;
}
static typeof(task) *current_task(void) {return &task;}
static void fd_problem(u64 why) {problems|=why;}
static u64 fd_file(struct file *file) {return file==&selected?23:0;}
static u64 fd_function_ip(struct pt_regs *ctx) {return ctx->ip;}
static void *lookup(void *map,const void *key) {
    return map==&ap_config_map && !*(const u32 *)key?&config:NULL;
}
static int fd_read_kernel(void *out,u32 length,const void *source) {
    if(read_failure || !source)return -1;
    const u64 address=(u64)source;
    const u64 prefix[]={0x4155415641574155ULL,0x0000a8ec81485354ULL,
        0x48f78949d4894900ULL,0xa8e8f631ed31fb89ULL,0xffffc4ULL};
    const u64 suffix[]={0xfff993d3e8df8948ULL,0x0000a8c48148e889ULL,0x5e415d415c415b00ULL,0xc35d5f41ULL};
    if(address==AP_STREAM_TX_IMAGE+5) {assert(length==35);memcpy(out,prefix,length);}
    else if(address==AP_STREAM_TX_IMAGE+0x855) {assert(length==28);memcpy(out,suffix,length);}
    else if(address>=AP_STREAM_TX_IMAGE && address<AP_STREAM_TX_IMAGE+sizeof(ap_tx_image_tcp_sendmsg)) {
        const u64 offset=address-AP_STREAM_TX_IMAGE;
        assert(offset<=sizeof(ap_tx_image_tcp_sendmsg)-length);
        memcpy(out,ap_tx_image_tcp_sendmsg+offset,length);
        if(bad_window && offset==(u64)bad_window)((u8 *)out)[length-1]^=1;
        return 0;
    }
    else {
        if(source==&stack[28]) {timeout_reads++;if(timeout_read_failure)return -1;}
        memcpy(out,source,length);return 0;
    }
    if(bad_image)((u8 *)out)[0]^=1;return 0;
}
struct stream_copy_skb_view {u64 head,data;u32 tail,end,size,nonlinear;s32 users;};
struct stream_copy_shared_view {u64 frag_list;s32 dataref;u8 count,flags;};
static int stream_copy_read_skb(struct sk_buff *s,struct stream_copy_skb_view *out) {
    if(!s || read_failure)return 0;
    *out=(struct stream_copy_skb_view){s->head,s->data,s->tail,s->end,s->len,s->data_len,s->users};return 1;
}
static int stream_copy_read_shared(struct skb_shared_info *s,struct stream_copy_shared_view *out) {
    assert((u8 *)s==head+1024);if(read_failure)return 0;
    *out=(struct stream_copy_shared_view){.dataref=1};return 1;
}
static void *stream_copy_reserve(void *map,u64 length,u64 flags) {
    assert(map==&stream_copy_records && length==sizeof(reserved) && !flags);
    return !ring_full && emitted_count<8?&reserved:NULL;
}
static void stream_copy_submit(void *record,u64 flags) {
    assert(record==&reserved && !flags);emitted[emitted_count++]=reserved;
}
static void stream_copy_discard(void *record,u64 flags) {assert(record==&reserved && !flags);}
static void stream_copy_record_init(struct ap_stream_copy_record *record,const struct ap_fd_call *call) {
    const struct ap_original_selection *s=&call->original.selection;
    record->provider=s->provider;record->command=s->command;record->call=s->call;
    record->task=s->task;record->task_start=s->task_start;
}
static void publish_result(struct ap_command_result *r) {
    assert(r==&receipt && r->phase==AP_COMMAND_RUNNING);r->phase=AP_COMMAND_DONE;published++;
}
#include "stream-tx.bpf.h"

/* The emitted bytes/COMMIT feed the actual collector, never a fabricated
 * success summary. Initial command/call custody remains a controlled premise. */
enum ap_slot_state {AP_SLOT_FREE,AP_SLOT_RESERVED,AP_SLOT_ACTIVE,AP_SLOT_DISARMING,AP_SLOT_COLLECTED,AP_SLOT_QUARANTINED};
struct ap_pending_command {
    enum ap_slot_state state;
    struct ap_task_command submitted;
    struct ap_command_result receipt;
    struct ap_fd_call original_receipt;
    union { struct ap_stream_tx_owned stream_tx;
        struct ap_stream_tx_blocking_owned stream_tx_blocking; };
    bool original_collected;
};
struct ap_session {u64 incarnation;struct ap_pending_command pending[AP_COMMANDS];};
static int enter_commands(struct ap_session *s) {return s?0:-1;}
static void leave_commands(struct ap_session *s) {(void)s;}
static int stream_copy_drain(struct ap_session *s) {(void)s;return 0;}
#include "stream-tx-driver.h"
static void reset(void) {
    memset(&active,0,sizeof(active));memset(&tcp,0,sizeof(tcp));memset(&skb,0,sizeof(skb));
    memset(head,0,sizeof(head));memcpy(head,"ABCdefgh",8);
    emitted_count=published=0;problems=0;timeout_reads=0;timeout_read_failure=false;bad_window=0;
    memset(stack,0,sizeof(stack));stack[28]=5000;
    missing_call=missing_result=ring_full=read_failure=bad_image=false;
    config=(struct ap_config){.anchor_phase=AP_GROUPED_ANCHOR_ACTIVE,.anchor_ip=AP_GROUPED_CONNECT_IMAGE};
    submitted=(struct ap_task_command){.provider=3,.command=7,.operation=AP_ORIGINAL_SENDTO_CALL,
        .expected_object=11,.generation_before=0x100000,.generation_after=19,
        .expected_level=5,.expected_option=0x4000,.original_count=8};
    receipt=(struct ap_command_result){.identity={.provider=3},.command=7,.operation=submitted.operation,
        .phase=AP_COMMAND_RUNNING,.task=13,.start_boottime=17,.original_count=8};
    task.files=&task;task.mm=&tcp;
    active.command=7;active.operation=submitted.operation;active.raw_table=(u64)task.files;active.new_file=(u64)task.mm;
    active.original.selection=(struct ap_original_selection){.provider=3,.command=7,.call=11,.owner_mm=19,
        .task=13,.task_start=17,.table=29,.requested_fd=5,.user_address=0x100000,.address_length=0x4000,.original_count=8};
    selected.f_flags=04000;socket_value=(struct socket){.sk=&tcp.sk,.file=&selected};
    tcp.sk.__sk_common.skc_state=1;tcp.sk.__sk_common.skc_family=2;
    tcp.sk.sk_protocol=6;tcp.sk.sk_type=1;tcp.sk.sk_socket=&socket_value;tcp.write_seq=100;
    tcp.sk.sk_write_queue.next=tcp.sk.sk_write_queue.prev=&tcp.sk.sk_write_queue;
    tcp.sk.tcp_rtx_queue.rb_node=&skb.rbnode;skb.rbnode.__rb_parent_color=1;
    skb.head=skb.data=(u64)head;skb.tail=skb.len=3;skb.end=1024;skb.users=1;
    *(struct tcp_skb_cb *)skb.cb=(struct tcp_skb_cb){.seq=100,.end_seq=103};
    message=(struct msghdr){.msg_flags=AP_STREAM_TX_FLAGS,.msg_iter={.count=8}};
}
static void phase(unsigned which,s64 returned) {
    struct pt_regs ctx={.ip=AP_STREAM_TX_IMAGE,.sp=(u64)&stack[40],.di=(u64)&tcp.sk,
        .si=(u64)&message,.dx=8,.ax=(u64)returned,.bp=(u64)returned};
    switch(which) {
    case 0:stream_tx_enter(&ctx);break;
    case 1:case 2:
        ctx.ip=AP_STREAM_TX_LOCK_IMAGE;ctx.sp-=AP_STREAM_TX_INNER_STACK;ctx.si=0;
        *(u64 *)ctx.sp=AP_STREAM_TX_IMAGE+AP_STREAM_TX_LOCK_RETURN;
        if(which==2)tcp.sk.sk_lock.owned=1;
        stream_tx_lock(&ctx,which==2);break;
    case 3:case 4:
        ctx.ip=AP_STREAM_TX_UNLOCK_IMAGE;ctx.sp-=AP_STREAM_TX_INNER_STACK;
        *(u64 *)ctx.sp=AP_STREAM_TX_IMAGE+AP_STREAM_TX_UNLOCK_RETURN;
        if(which==3)tcp.write_seq=100+(returned>0?(u32)returned:0);
        else tcp.sk.sk_lock.owned=0;
        stream_tx_unlock(&ctx,which==4);break;
    case 5:stream_tx_exit(&ctx);break;
    case 6:
        ctx=(struct pt_regs){.orig_ax=44,.cs=0x33,.di=5,.si=0x100000,.dx=8,.r10=0x4000};
        {u64 args[]={(u64)&ctx,(u64)returned};stream_tx_syscall_exit(args,&submitted);}break;
    default:assert(0);
    }
}
static void run(unsigned omitted,unsigned duplicated,s64 returned) {
    for(unsigned i=0;i<7;i++)if(!(omitted&(1U<<i))) {
        phase(i,returned);if(duplicated&(1U<<i))phase(i,returned);
    }
}
static void no_completion(void) {
    assert(!published && !active.original.complete && receipt.phase==AP_COMMAND_RUNNING);
    for(unsigned i=0;i<emitted_count;i++)assert(emitted[i].kind!=AP_STREAM_TX_COMMIT);
    checks++;
}
static void positive(s64 returned) {
    reset();run(0,0,returned);
    assert(!problems && !active.original.problem && published==1 && active.original.complete==1);
    assert(receipt.returned==returned && receipt.phase==AP_COMMAND_DONE);
    assert(ap_original_result_matches(&submitted,&receipt,&active.original));
    struct ap_session s={.incarnation=3};
    struct ap_pending_command *p=&s.pending[ap_command_slot(7)];
    p->state=AP_SLOT_ACTIVE;p->submitted=submitted;
    for(unsigned i=0;i<emitted_count;i++)assert(!stream_tx_record(p,&emitted[i]));
    assert(p->stream_tx.committed);
    p->receipt=receipt;p->original_receipt=active;p->state=AP_SLOT_COLLECTED;p->original_collected=true;
    struct ap_stream_tx_capture out;
    assert(!ap_original_sendto_capture(&s,7,&out) && out.returned==returned);
    assert(out.summary.captured==(returned>0?(u64)returned:0));
    if(returned>0)assert(returned==3 && !memcmp(out.bytes,"ABC",3));
    checks++;
}
static void blocking_reset(void) {
    reset();submitted.operation=AP_ORIGINAL_SENDTO_BLOCKING_CALL;
    submitted.expected_timeout_ticks=5000;
    receipt.operation=active.operation=submitted.operation;
    selected.f_flags=0;message.msg_flags=0x4000;
}
static void positive_blocking(int wrap) {
    blocking_reset();
    if(wrap) {
        tcp.write_seq=0xfffffffeU;
        *(struct tcp_skb_cb *)skb.cb=(struct tcp_skb_cb){.seq=0xfffffffeU,.end_seq=1};
    }
    for(unsigned i=0;i<7;i++) {
        if(i==3 && wrap) {
            struct pt_regs ctx={.ip=AP_STREAM_TX_UNLOCK_IMAGE,.sp=(u64)&stack[12],
                .di=(u64)&tcp.sk,.bp=3};
            stack[12]=AP_STREAM_TX_IMAGE+AP_STREAM_TX_UNLOCK_RETURN;tcp.write_seq=1;
            stream_tx_unlock(&ctx,0);
        } else phase(i,3);
    }
    assert(!problems && !active.original.problem && published==1 && timeout_reads==1);
    assert(ap_original_result_matches(&submitted,&receipt,&active.original));
    struct ap_session owner={.incarnation=3};struct ap_pending_command *p=&owner.pending[ap_command_slot(7)];
    p->state=AP_SLOT_ACTIVE;p->submitted=submitted;
    for(unsigned i=0;i<emitted_count;i++)assert(!stream_tx_blocking_record(p,&emitted[i]));
    p->receipt=receipt;p->original_receipt=active;p->state=AP_SLOT_COLLECTED;p->original_collected=true;
    struct {u64 pre;struct ap_stream_tx_blocking_capture out;u64 post;} guarded={.pre=0xfeed,.post=0xbeef};
    assert(!ap_original_sendto_blocking_capture(&owner,7,&guarded.out));
    assert(guarded.pre==0xfeed && guarded.post==0xbeef && guarded.out.returned==3 &&
        guarded.out.summary.prefix.version==2 && guarded.out.summary.saved_timeout_ticks==5000 &&
        !memcmp(guarded.out.bytes,"ABC",3));
    struct ap_stream_tx_capture old;
    assert(ap_original_sendto_capture(&owner,7,&old)==-1);
    checks++;
}
static void blocking_controls(void) {
    positive_blocking(0);positive_blocking(1);
    /* Keep every old127 omission case; additionally run every subset on v2. */
    for(unsigned mask=1;mask<128;mask++) {blocking_reset();run(mask,0,3);no_completion();}
    for(unsigned which=0;which<4;which++) {
        blocking_reset();for(unsigned i=0;i<3;i++)phase(i,3);
        struct pt_regs inner={.ip=AP_STREAM_TX_UNLOCK_IMAGE,.sp=(u64)&stack[12],
            .di=(u64)&tcp.sk,.bp=3};
        stack[12]=AP_STREAM_TX_IMAGE+0xb34; /* actual wait-helper call, never final */
        stream_tx_unlock(&inner,0);
        if(which&1)stream_tx_unlock(&inner,1);
        if(which&2) {
            inner.ip=AP_STREAM_TX_LOCK_IMAGE;inner.si=0;stream_tx_lock(&inner,0);
            if(which&1)stream_tx_lock(&inner,1);
        }
        for(unsigned i=3;i<7;i++)phase(i,3);
        assert(problems && active.original.problem && timeout_reads==0 && stack[28]==5000);
        no_completion();
    }
    /* A duplicate initial-lock ENTRY is independently fatal even without its
     * return and without an earlier inner unlock in the modeled sequence. */
    for(unsigned returned=0;returned<2;returned++) {
        blocking_reset();for(unsigned i=0;i<3;i++)phase(i,3);
        phase(1,3);if(returned)phase(2,3);
        for(unsigned i=3;i<7;i++)phase(i,3);
        assert(problems && active.original.problem && !timeout_reads);no_completion();
    }
    for(unsigned which=0;which<7;which++) {
        blocking_reset();for(unsigned i=0;i<6;i++)phase(i,3);
        struct pt_regs ctx={.orig_ax=44,.cs=0x33,.di=5,.si=0x100000,.dx=8,.r10=0x4000};
        u64 *operands[]={&ctx.cs,&ctx.di,&ctx.si,&ctx.dx,&ctx.r10,&ctx.r8,&ctx.r9};
        (*operands[which])++;u64 args[]={(u64)&ctx,3};stream_tx_syscall_exit(args,&submitted);
        assert(problems);no_completion();
    }
    blocking_reset();timeout_read_failure=true;run(0,0,3);
    assert(timeout_reads==1 && problems);no_completion();
    for(unsigned which=0;which<17;which++) {
        blocking_reset();
        switch(which) {
        case 0:tcp.inet_flags=AP_STREAM_TX_DEFER_CONNECT_MASK;break;
        case 1:message.msg_flags|=0x20000000;break;
        case 2:tcp.inet_flags=AP_STREAM_TX_DEFER_CONNECT_MASK;message.msg_flags|=0x20000000;break;
        case 3:message.msg_flags|=0x4000000;break;
        case 4:message.msg_flags|=0x8000000;break;
        case 5:message.msg_control=&task;message.msg_controllen=8;break;
        case 6:selected.f_flags=04000;break;
        case 7:submitted.expected_timeout_ticks=0;break;
        case 8:submitted.expected_timeout_ticks=0x7fffffffffffffffULL;break;
        case 9:stack[28]=5001;break;
        case 10:stack[28]=0;break;
        case 11:tcp.sk.__sk_common.skc_state=3;break;
        case 12:bad_window=AP_TX_PREFIX_OFFSET;break;
        case 13:bad_window=AP_TX_LOAD_OFFSET;break;
        case 14:bad_window=AP_TX_WAIT_MEMORY_OFFSET;break;
        case 15:bad_window=AP_TX_WAIT_CONNECT_OFFSET;break;
        case 16:tcp.repair=1;break;
        }
        run(0,0,3);assert(problems);no_completion();
    }
    for(s64 returned=-14;returned<=0;returned+=14) {
        blocking_reset();run(0,0,returned);assert(problems && !timeout_reads);no_completion();
    }
    blocking_reset();run(0,0,9);assert(problems && !timeout_reads);no_completion();
}
int main(void) {
    positive(3);positive(0);positive(-14);
    /* All nonempty omission subsets include paired and multiple missing inner
     * callbacks. No historical-global-counter observation grants completion. */
    for(unsigned mask=1;mask<128;mask++) {reset();run(mask,0,3);no_completion();}
    for(unsigned i=0;i<6;i++) {reset();run(0,1U<<i,3);assert(problems);no_completion();}
    reset();run(0,1U<<6,3);assert(published==1 && problems && active.original.problem);
    assert(!ap_original_result_matches(&submitted,&receipt,&active.original));checks++;
    for(unsigned which=0;which<16;which++) {
        reset();
        switch(which) {
        case 0:missing_call=true;break;
        case 1:missing_result=true;break;
        case 2:active.raw_table++;break;
        case 3:active.new_file++;break;
        case 4:receipt.task++;break;
        case 5:active.original.selection.call++;break;
        case 6:active.original.selection.original_count++;break;
        case 7:message.msg_flags|=1;break;
        case 8:message.msg_iter.count++;break;
        case 9:tcp.repair=1;break;
        case 10:selected.f_flags=0;break;
        case 11:bad_image=true;break;
        case 12:read_failure=true;break;
        case 13:ring_full=true;break;
        case 14:skb.rbnode.rb_left=&skb.rbnode;break;
        case 15:((struct tcp_skb_cb *)skb.cb)->seq++;break;
        }
        run(0,0,3);assert(problems);no_completion();
    }
    reset();for(unsigned i=0;i<5;i++)phase(i,3);phase(5,4);phase(6,3);no_completion();
    reset();for(unsigned i=0;i<6;i++)phase(i,3);phase(6,4);no_completion();
    /* Original operand mismatch at actual syscall EXIT cannot be repaired by
     * earlier fully valid protocol receipts or already staged accepted bytes. */
    for(unsigned which=0;which<7;which++) {
        reset();for(unsigned i=0;i<6;i++)phase(i,3);
        struct pt_regs ctx={.orig_ax=44,.cs=0x33,.di=5,.si=0x100000,.dx=8,.r10=0x4000};
        u64 *operands[]={&ctx.cs,&ctx.di,&ctx.si,&ctx.dx,&ctx.r10,&ctx.r8,&ctx.r9};
        (*operands[which])++;u64 args[]={(u64)&ctx,3};stream_tx_syscall_exit(args,&submitted);
        assert(problems);no_completion();
    }
    blocking_controls();
    printf("actual TX callbacks+collector: %u controls, all127 omission subsets and7 duplicate stages; native UNRUN\n",checks);
    return 0;
}
