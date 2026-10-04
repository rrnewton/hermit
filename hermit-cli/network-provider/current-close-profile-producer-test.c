/* SPDX-License-Identifier: BSD-3-Clause */
/* Actual callbacks; explicit controlled kernel memory/map/CO-RE premises. */
#define AP_GROUPED_PROVIDER 1
#define AP_FTRACE_PROVIDER 1
#define AP_CURRENT_CLOSE_PROFILE_ENABLED 1
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include "current-close-profile.h"
#define CORE(x) (x)
#define ARRAY(name,type,count) static int name
#define BPF_FUNC_probe_read_user 112
#define SOCK_LINGER 4
struct inode {int present;};
struct file_operations {u64 flush,release;};
struct proto_ops {u64 release;};
struct proto {u64 close;};
struct sock;
struct socket {struct file *file;struct sock *sk;const struct proto_ops *ops;};
struct sock {struct {u16 skc_family;struct proto *skc_prot;unsigned long skc_flags;} __sk_common;
    u16 sk_type,sk_protocol;unsigned long sk_lingertime;struct socket *sk_socket;};
struct inet_connection_sock {struct sock sk;void *icsk_ulp_ops,*icsk_ulp_data;};
struct tcp_sock {struct inet_connection_sock icsk;u64 repair;};
struct file {struct {struct {s64 counter;} refcnt;} f_ref;struct inode *f_inode;
    const struct file_operations *f_op;void *private_data;};
struct fdtable {u32 max_fds;struct file **fd;};
struct files_struct {struct {s32 counter;} count;struct fdtable *fdt;};
struct mm_struct {int present;};
struct task_struct {struct mm_struct *mm;struct files_struct *files;u64 start_boottime,jobctl;
    u32 __state,ptrace;struct task_struct *parent;};
/* The host control supplies the same decoded one-bit value. Real CO-RE
 * relocation/bitfield layout is checked by the separate BPF compiler. */
#define __builtin_preserve_field_info(field,kind) ((kind)==0?offsetof(struct tcp_sock,repair):((kind)==1?8:0))
struct ap_fd_file {u64 identity;};
static struct ap_close_profile row;
static struct ap_task_command submitted;
static struct ap_command_result completion;
static struct ap_config config;
static int ap_config_map,fd_files;
static struct ap_fd_file enrolled_file;
static struct task_struct target,tracer;
static struct mm_struct mm;
static struct files_struct files;
static struct fdtable fdt;
static struct file *slots[64],file;
static struct inode inode;
static struct socket socket;
static struct tcp_sock tcp;
static struct file_operations fops;
static struct proto_ops ops;
static struct proto prot;
static struct iovec iov;
static u64 regs[27],failures;
static unsigned checks,published,reads,fail_read_at;
static bool missing_row,missing_result,missing_config,missing_file,user_error,wrong_table;
static void *lookup(void *,const void *);
static struct ap_command_result *result(u64 command) {return command==7&&!missing_result?&completion:NULL;}
static struct task_struct *current_task(void) {return &tracer;}
static u64 pid_tgid(void) {return 23;}
static u64 incarnation(void) {return 3;}
static u64 fd_target_task(struct task_struct *task) {return task==&target?17:0;}
static u64 fd_table(struct files_struct *owner,int create) {assert(owner==&files&&!create);return wrong_table?99:11;}
static void ap_fail(u64 bit) {failures|=bit;}
static void publish_result(struct ap_command_result *r) {assert(r==&completion&&r->phase==AP_COMMAND_RUNNING);r->phase=AP_COMMAND_DONE;published++;}
static long fd_read_kernel(void *out,u32 bytes,const void *address) {
    reads++;if(reads==fail_read_at)return -14;
    u64 addr=(u64)address;
#define TRANSLATE(base,object) if(addr>=(base)&&addr-(base)+bytes<=sizeof(object))address=(char *)&(object)+(addr-(base))
    TRANSLATE(AP_CLOSE_SOCKET_FILE_OPS_IMAGE,fops);
    TRANSLATE(AP_CLOSE_INET_STREAM_OPS_IMAGE,ops);
    TRANSLATE(AP_CLOSE_TCP_PROT_IMAGE,prot);
#undef TRANSLATE
    assert(address);memcpy(out,address,bytes);return 0;
}
#include "current-close-profile.bpf.h"
static void *lookup(void *map,const void *key) {
    if(map==&ap_config_map && *(const u32 *)key==0)return missing_config?NULL:&config;
    if(map==&current_close_profiles && *(const u32 *)key==7)return missing_row?NULL:&row;
    if(map==&fd_files && *(const u64 *)key==(u64)&file)return missing_file?NULL:&enrolled_file;
    assert(0);return NULL;
}
static long read_user(void *out,u32 bytes,const void *address) {
    if(user_error)return -14;
    assert((address==&iov && bytes==sizeof(iov)) ||
        ((address==&regs[14] || address==&regs[15])&&bytes==8));
    memcpy(out,address,bytes);return 0;
}
static void reset(void) {
    missing_row=missing_result=missing_config=missing_file=user_error=wrong_table=false;
    checks+=0;published=reads=fail_read_at=0;failures=0;
    memset(slots,0,sizeof(slots));memset(&tcp,0,sizeof(tcp));memset(regs,0,sizeof(regs));
    fops=(struct file_operations){0,AP_CLOSE_SOCK_CLOSE_IMAGE};ops=(struct proto_ops){AP_CLOSE_INET_RELEASE_IMAGE};
    prot=(struct proto){AP_CLOSE_TCP_CLOSE_IMAGE};
    file=(struct file){.f_ref={{1}},.f_inode=&inode,.f_op=(void *)AP_CLOSE_SOCKET_FILE_OPS_IMAGE,.private_data=&socket};
    socket=(struct socket){&file,&tcp.icsk.sk,(void *)AP_CLOSE_INET_STREAM_OPS_IMAGE};
    tcp.icsk.sk=(struct sock){.__sk_common={2,(void *)AP_CLOSE_TCP_PROT_IMAGE,0},.sk_type=1,.sk_protocol=6,.sk_socket=&socket};
    slots[3]=slots[4]=&file;fdt=(struct fdtable){64,slots};files=(struct files_struct){{1},&fdt};
    tracer=(struct task_struct){.start_boottime=29};
    target=(struct task_struct){.mm=&mm,.files=&files,.start_boottime=19,.parent=&tracer,.ptrace=1,
        .__state=AP_TASK_TRACED,.jobctl=AP_JOBCTL_FROZEN|AP_JOBCTL_TRACED};
    iov=(struct iovec){regs,sizeof(regs)};regs[14]=4;regs[15]=3;enrolled_file.identity=12;
    config=(struct ap_config){.provider=3,.anchor_phase=AP_GROUPED_ANCHOR_ACTIVE,.anchor_ip=AP_GROUPED_CONNECT_IMAGE};
    submitted=(struct ap_task_command){.provider=3,.command=7,.operation=27,.expected_object=11,
        .generation_before=13,.generation_after=0,.expected_level=AP_PTRACE_GETREGSET,.expected_option=1};
    completion=(struct ap_command_result){.command=7,.operation=27,.phase=AP_COMMAND_READY};
    row=(struct ap_close_profile){.intent={7,13,0,0,11,12,4,0,3}};close_read_user=read_user;
}
static void phase(bool exit,s64 raw) {
    u64 ctx[]={(u64)&target,AP_PTRACE_GETREGSET,AP_NT_PRSTATUS,(u64)&iov,(u64)raw};
    if(exit)current_close_profile_returned(ctx,&submitted);else current_close_profile_enter(ctx,&submitted);
}
#define CHECK(x) do {assert(x);checks++;} while(0)
#define VALID() ap_close_profile_finite(&submitted,&completion,&row,AP_GROUPED_CONNECT_IMAGE)
#define SETTLED() ap_close_profile_matches(&submitted,&completion,&row)
int main(void) {
    reset();phase(false,0);CHECK(!VALID() && !published && row.phases==3);phase(true,0);
    CHECK(VALID() && published==1 && !failures && row.entered.aliases==2 && row.entered.file_refs==2);
    unsigned successful_reads=reads;
    reset();slots[3]=NULL;file.f_ref.refcnt.counter=0;phase(false,0);phase(true,0);
    CHECK(VALID() && row.entered.aliases==1 && row.entered.file_refs==1);
    for(unsigned read=1;read<=successful_reads;read++) {
        reset();fail_read_at=read;phase(false,0);phase(true,0);CHECK(!VALID() && row.problem&AP_CLOSE_READ);
    }
    for(unsigned unsafe=0;unsafe<7;unsafe++) {
        reset();
        switch(unsafe) {case 0:tcp.icsk.sk.__sk_common.skc_flags=1ULL<<SOCK_LINGER;break;
        case 1:tcp.repair=1;break;case 2:tcp.icsk.icsk_ulp_ops=&mm;break;
        case 3:tcp.icsk.icsk_ulp_data=&mm;break;case 4:prot.close++;break;
        case 5:fops.flush=1;break;case 6:ops.release++;break;}
        phase(false,0);phase(true,0);CHECK(SETTLED() && !VALID() && row.problem==AP_CLOSE_UNSUPPORTED);
    }
    for(unsigned bad=0;bad<14;bad++) {
        reset();
        switch(bad) {case 0:missing_row=true;break;case 1:missing_result=true;break;case 2:missing_config=true;break;
        case 3:missing_file=true;break;case 4:user_error=true;break;case 5:wrong_table=true;break;
        case 6:target.parent=NULL;break;case 7:target.jobctl=AP_JOBCTL_TRACED;break;case 8:files.count.counter=2;break;
        case 9:enrolled_file.identity++;break;case 10:regs[15]=45;break;case 11:regs[14]=3;break;
        case 12:file.f_ref.refcnt.counter=2;break;case 13:iov.iov_len=215;break;}
        phase(false,0);if(completion.phase==AP_COMMAND_RUNNING)phase(true,0);CHECK(!VALID());
    }
    reset();phase(false,0);phase(true,-14);CHECK(!SETTLED() && completion.returned==-14 && row.ptrace_return==-14);
    reset();phase(false,0);target.start_boottime++;phase(true,0);CHECK(failures && !published);
    reset();phase(false,0);phase(true,0);phase(true,0);CHECK(failures && published==1);
    reset();u64 other[]={(u64)&target,AP_PTRACE_GETREGSET,0x202,(u64)&iov,0};
    current_close_profile_enter(other,&submitted);current_close_profile_returned(other,&submitted);
    CHECK(!row.phases && !published && !reads && !failures);
    printf("current Close actual producer: %u checks\n",checks);return 0;
}
