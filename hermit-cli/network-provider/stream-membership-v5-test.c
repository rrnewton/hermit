/* SPDX-License-Identifier: MIT */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#define AP_NATIVE_COPY_VERSION 5
#include "fd-effects.h"
#include "stream-frontier.h"
struct file {u64 reserved;};
struct sock;
struct proto {void *recvmsg;};
struct socket {struct file *file;struct sock *sk;};
struct sock_common {struct proto *skc_prot;};
struct sock {struct sock_common __sk_common;struct socket *sk_socket;};
struct msghdr {u64 msg_iter;};
struct unix_stream_read_state {
    void *recv_actor;struct socket *socket;struct msghdr *msg;void *pipe;
    u64 size;s32 flags;u32 splice_flags;
};
struct pt_regs {u64 di,si,dx,cx,sp,ip,ax,function,cookie;};
#define CORE(x) (x)
static int fd_files;
static struct file file;
static struct socket socket;
static struct sock sock;
static struct proto protocol_ops;
static struct msghdr message;
static struct unix_stream_read_state state;
static struct ap_fd_file known;
static struct ap_task_command owner;
static struct ap_fd_call call;
static int tracked,has_command,has_call,pending,looked,commands,reads,fail_read;
static u64 problem;
static void *lookup(void *map,const u64 *key) {
    assert(map==&fd_files);assert(*key==(u64)&file);looked++;
    return tracked?&known:NULL;
}
static struct ap_task_command *command(void) {assert(looked);commands++;return has_command?&owner:NULL;}
static struct ap_fd_call *stream_copy_call(void) {assert(looked);return has_call?&call:NULL;}
static struct ap_fd_call *stream_copy_recv_pending(const struct ap_task_command *c) {
    assert(c==&owner && looked);return pending?&call:NULL;
}
static void fd_problem(u64 value) {problem|=value;}
static void stream_copy_problem(struct ap_fd_call *c,u64 value) {assert(c==&call);c->original.problem|=value;problem|=value;}
static long fd_read_kernel(void *out,u32 size,const void *from) {
    reads++;if(!from || reads==fail_read)return -1;memcpy(out,from,size);return 0;
}
static u64 fd_function_ip(struct pt_regs *ctx) {return ctx->function;}
static u64 fd_attach_cookie(struct pt_regs *ctx) {return ctx->cookie;}
#include "stream-membership.bpf.h"
static unsigned checks;
#define CHECK(x) do {assert(x);checks++;} while(0)
static struct pt_regs reset(unsigned cookie) {
    tracked=has_command=has_call=1;pending=looked=commands=reads=fail_read=0;problem=0;
    file=(struct file){0};protocol_ops=(struct proto){0};
    socket=(struct socket){.file=&file,.sk=&sock};
    sock=(struct sock){.__sk_common={.skc_prot=&protocol_ops},.sk_socket=&socket};
    known=(struct ap_fd_file){.identity=31};
    assert(ap_stream_frontier_enroll_install(&known,31,19,23));
    owner=(struct ap_task_command){.operation=AP_ORIGINAL_READ};
    call=(struct ap_fd_call){.selected_file=31,.selection.word=(u64)&file,
        .original.selection.ready=1};
    call.original.stream_copy.iterator=(u64)&message.msg_iter;
    call.original.stream_copy.summary.initial_count=9;
    state=(struct unix_stream_read_state){.socket=&socket,.msg=&message,.size=9};
    return (struct pt_regs){.di=cookie==1||cookie==2||cookie==3||cookie==8||cookie==17?(u64)&socket:
        cookie==7?(u64)&state:(u64)&sock,.si=(u64)&message,.dx=9};
}
static void unauthorized(void) {
    for(unsigned cookie=1;cookie<=20;cookie++)if(cookie!=4) {
        struct pt_regs ctx=reset(cookie);has_command=0;
        CHECK(stream_membership_entry(&ctx,cookie)==0);CHECK(looked==1);CHECK(commands==1);
        CHECK(problem==AP_FD_OUTCOME);CHECK(!call.security_socket);
        ctx=reset(cookie);owner.operation=AP_ORIGINAL_FILE;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_OUTCOME);
        ctx=reset(cookie);tracked=0;has_command=0;
        CHECK(stream_membership_entry(&ctx,cookie)==(cookie<=3));CHECK(!commands);CHECK(!problem);
    }
    for(unsigned cookie=1;cookie<=3;cookie++) {
        struct pt_regs ctx=reset(cookie);has_call=0;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_OUTCOME);
        ctx=reset(cookie);known.identity=0;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_IDENTITY);
        ctx=reset(cookie);call.selected_file++;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_OUTCOME);
        ctx=reset(cookie);call.selection.word+=8;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_OUTCOME);
        ctx=reset(cookie);CHECK(stream_membership_entry(&ctx,cookie)==1);CHECK(!problem);
        call.security_socket=ap_stream_frame_begin(cookie,0x1000);
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_DUPLICATE);
    }
    for(unsigned fail=1;fail<=2;fail++) {
        struct pt_regs ctx=reset(5);fail_read=(int)fail;
        CHECK(!stream_membership_entry(&ctx,5));CHECK(problem==AP_FD_IDENTITY);CHECK(!commands);
    }
    for(unsigned cookie=6;cookie<=20;cookie++)if(cookie!=7) {
        struct pt_regs ctx=reset(cookie);call.security_socket=ap_stream_frame_advance(ap_stream_frame_begin(1,0x1000),1,2);
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(problem==AP_FD_OUTCOME);
    }
}
static void tcp(void) {
    for(unsigned protocol=1;protocol<=2;protocol++) {
        struct pt_regs ctx=reset(5);
        const u64 function=protocol==1?0xffffffff82355dc0ULL:0xffffffff820524a0ULL;
        call.original.stream_copy.protocol_ip=function;
#ifdef AP_FTRACE_PROVIDER
        call.security_socket=ap_stream_frame_begin(protocol,0x1000);
        protocol_ops.recvmsg=(void *)ap_stream_tcp_target(protocol,function);
        looked=1; /* production caller already authenticated known */
        CHECK(stream_membership_wrapper_admitted(&known,
            stream_membership_wrapper_dispatch(&call,&socket,&message,9,0)));
#else
        call.security_socket=ap_stream_frame_advance(ap_stream_frame_begin(protocol,0x1000),protocol,2);
#endif
        ctx.sp=0x1000;ctx.function=ap_stream_tcp_target(protocol,function);
        CHECK(!stream_membership_entry(&ctx,5));CHECK(!problem);CHECK(ap_stream_frame_stage(call.security_socket)==3);
        CHECK(!stream_membership_entry(&ctx,5));CHECK(problem==AP_FD_OUTCOME);
        for(unsigned wrong=0;wrong<6;wrong++) {
            ctx=reset(5);call.original.stream_copy.protocol_ip=function;
            call.security_socket=ap_stream_frame_advance(ap_stream_frame_begin(protocol,0x1000),protocol,2);
            ctx.sp=0x1000;ctx.function=ap_stream_tcp_target(protocol,function);
            if(wrong==0)ctx.sp+=8;if(wrong==1)ctx.function++;
            if(wrong==2)ctx.si++;if(wrong==3)ctx.dx++;
            if(wrong==4)ctx.cx=2;if(wrong==5)call.security_socket=ap_stream_frame_begin(protocol,0x1000);
            CHECK(!stream_membership_entry(&ctx,5));CHECK(problem==AP_FD_OUTCOME);
        }
#ifndef AP_FTRACE_PROVIDER
        ctx=reset(5);call.original.stream_copy.protocol_ip=function;
        call.security_socket=ap_stream_frame_begin(protocol,0x1000);
        ctx.sp=0x1000;ctx.ip=function+AP_STREAM_DISPATCH_OFFSET+1;
        ctx.cookie=protocol==1?AP_STREAM_DISPATCH4_COOKIE:AP_STREAM_DISPATCH6_COOKIE;
        ctx.ax=ap_stream_tcp_target(protocol,function);
        CHECK(!stream_membership_dispatch(&ctx));CHECK(!problem);CHECK(ap_stream_frame_stage(call.security_socket)==2);
        /* A clean stage-2 callback can be the retiring classic observer
         * confirming a frame already advanced by the ftrace replacement. */
        CHECK(!stream_membership_dispatch(&ctx));CHECK(!problem);
        CHECK(ap_stream_frame_stage(call.security_socket)==2);
#endif
    }
}
static void unix_path(void) {
    _Alignas(8) unsigned char frame[256];
    for(unsigned wrong=0;wrong<9;wrong++) {
        struct pt_regs ctx=reset(7);memset(frame,0,sizeof(frame));
        const u64 function=0xffffffff8215a110ULL,entry=(u64)frame+128;
        struct unix_stream_read_state *actual=(void *)(entry-48);
        *actual=state;actual->recv_actor=(void *)(function+0xb70);
        u64 returned=function+0x52;memcpy((void *)(entry-56),&returned,8);
        call.original.stream_copy.protocol_ip=function;call.security_socket=ap_stream_frame_begin(3,entry);
        ctx.di=(u64)actual;ctx.si=1;ctx.sp=entry-56;ctx.function=function+0x70;
        if(wrong==1)ctx.function++;if(wrong==2)ctx.sp-=8;
        if(wrong==3)actual->recv_actor=(void *)(function+0xb71);
        if(wrong==4)actual->pipe=&file;if(wrong==5)actual->splice_flags=1;
        if(wrong==6)actual->flags=2;if(wrong==7)actual->size++;
        if(wrong==8) {returned++;memcpy((void *)(entry-56),&returned,8);}
        CHECK(!stream_membership_entry(&ctx,7));
        if(!wrong) {CHECK(!problem);CHECK(ap_stream_frame_stage(call.security_socket)==3);CHECK((call.security_socket&7)==0);}
        else CHECK(problem==AP_FD_OUTCOME);
    }
}
static void sticky_frontier_membership(void) {
    for(unsigned cookie=1;cookie<=20;cookie++)if(cookie!=4) {
        struct pt_regs ctx=reset(cookie);known.stream_state=0;
        known.stream_birth_command=known.stream_birth_install=0;
        CHECK(!stream_membership_entry(&ctx,cookie));CHECK(!commands);
        CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
        CHECK(!ap_stream_frontier_enroll_install(&known,31,19,23));
        CHECK(!known.stream_bytes && !known.stream_units);
        ctx=reset(cookie);has_command=0;
        CHECK(!stream_membership_entry(&ctx,cookie));
        CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
        has_command=1;CHECK(!stream_membership_entry(&ctx,cookie));
        CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
    }
    for(unsigned mutation=0;mutation<6;mutation++) {
        struct pt_regs ctx=reset(1);
        if(mutation==0)known.identity=0;
        if(mutation==1)has_call=0;
        if(mutation==2)call.selected_file++;
        if(mutation==3)call.selection.word+=8;
        if(mutation==4)call.security_socket=ap_stream_frame_begin(1,0x1000);
        if(mutation==5)owner.operation=AP_ORIGINAL_FILE;
        CHECK(!stream_membership_entry(&ctx,1));CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
        CHECK(!known.stream_bytes && !known.stream_units);
    }
    for(unsigned mutation=0;mutation<5;mutation++) {
#ifndef AP_FTRACE_PROVIDER
        struct pt_regs ctx=reset(5);ctx.cookie=AP_STREAM_DISPATCH4_COOKIE;
        ctx.function=0xffffffff82355dc0ULL;ctx.ip=ctx.function+AP_STREAM_DISPATCH_OFFSET+1;
        ctx.sp=0x1000;ctx.ax=ap_stream_tcp_target(1,ctx.function);
        call.original.stream_copy.protocol_ip=ctx.function;
        call.security_socket=ap_stream_frame_begin(1,ctx.sp);
        if(mutation==0)has_call=0;
        if(mutation==1)call.selected_file++;
        if(mutation==2)ctx.ax++;
        if(mutation==3)ctx.sp+=8;
        if(mutation==4)known.identity=0;
        CHECK(!stream_membership_dispatch(&ctx));CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
#else
        reset(5);const u64 function=0xffffffff82355dc0ULL;
        call.original.stream_copy.protocol_ip=function;
        call.security_socket=ap_stream_frame_begin(1,0x1000);
        protocol_ops.recvmsg=(void *)ap_stream_tcp_target(1,function);
        struct msghdr *observed=&message;u64 length=9;s32 flags=0;
        if(mutation==0)protocol_ops.recvmsg=(void *)((u64)protocol_ops.recvmsg+1);
        if(mutation==1)observed=0;
        if(mutation==2)length++;
        if(mutation==3)flags=2;
        if(mutation==4)call.security_socket=ap_stream_frame_advance(
            call.security_socket,1,2);
        looked=1; /* production caller already authenticated known */
        CHECK(!stream_membership_wrapper_admitted(&known,
            stream_membership_wrapper_dispatch(&call,&socket,observed,length,flags)));
        CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
#endif
        CHECK(!known.stream_bytes && !known.stream_units);
    }
    // A positive dispatch/locked entry preserves enrollment. A later malformed
    // observed concrete entry poisons it; restoring operands cannot clear it.
    struct pt_regs ctx=reset(5);ctx.function=0xffffffff82355dc0ULL;
    call.original.stream_copy.protocol_ip=ctx.function;
    call.security_socket=ap_stream_frame_advance(ap_stream_frame_begin(1,0x1000),1,2);
    ctx.function=ap_stream_tcp_target(1,ctx.function);ctx.sp=0x1000;
    CHECK(!stream_membership_entry(&ctx,5));CHECK(!problem);
    CHECK(known.stream_state==AP_STREAM_FRONTIER_ENROLLED);
    ctx.sp+=8;CHECK(!stream_membership_entry(&ctx,5));
    CHECK(known.stream_state&AP_STREAM_FRONTIER_POISON);
}
int main(void) {
#ifdef AP_FTRACE_PROVIDER
    (void)fd_attach_cookie;
#endif
    (void)ap_require_retirement_target;
    (void)stream_membership_wrapper_dispatch;
    (void)stream_membership_wrapper_admitted;
    sticky_frontier_membership();
    unauthorized();tcp();unix_path();
    printf("COPY5_FRONTIER_MEMBERSHIP controls=%u routes=19 file_first=1 sticky_row=1 native_effects=0\n",checks);return 0;
}
