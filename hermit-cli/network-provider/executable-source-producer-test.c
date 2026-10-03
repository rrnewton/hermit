/* SPDX-License-Identifier: BSD-3-Clause */
/* Actual production callbacks, controlled kernel/helper/map boundary. */
#define AP_GROUPED_PROVIDER 1
#define AP_FTRACE_PROVIDER 1
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include "executable-source.h"
#define CORE(x) (x)
#define ARRAY(name,type,count) static int name
#define BPF_FUNC_find_vma 180
#define BPF_FUNC_probe_read_user 112
struct super_block {u64 s_magic,s_dev;};
struct inode {struct super_block *i_sb;u64 i_ino;s64 i_size;u32 i_mode;struct {s32 counter;} i_writecount;};
struct address_space {struct inode *host;};
struct file {struct inode *f_inode;struct address_space *f_mapping;void *f_op;u32 f_mode;};
struct mm_struct {struct file *exe_file;};
struct task_struct {struct mm_struct *mm;u64 start_boottime,jobctl;u32 __state,ptrace;struct task_struct *parent;};
struct vm_area_struct {struct mm_struct *vm_mm;struct file *vm_file;void *vm_ops;u64 vm_start,vm_end,vm_pgoff,vm_flags;struct {void *ctx;} vm_userfaultfd_ctx;};
static struct ap_executable_source row;
static struct ap_task_command submitted;
static struct ap_command_result completion;
static struct ap_config config;
static int ap_config_map;
static struct task_struct target,tracer;
static struct mm_struct mm;
static struct super_block sb;
static struct inode inode;
static struct address_space mapping;
static struct file file;
static struct vm_area_struct vma;
static struct iovec iov;
static unsigned checks,published,find_calls;
static u64 failures;
static bool missing_row,missing_result,missing_config,user_error,no_callback,double_callback;
static s64 helper_result;
static void *lookup(void *map,const void *key);
static struct ap_command_result *result(u64 command) {return command==7&&!missing_result?&completion:NULL;}
static struct task_struct *current_task(void) {return &tracer;}
static u64 pid_tgid(void) {return 23;}
static u64 incarnation(void) {return 3;}
static u64 fd_target_task(struct task_struct *task) {return task==&target?17:0;}
static void ap_fail(u64 bit) {failures|=bit;}
static void publish_result(struct ap_command_result *r) {assert(r==&completion && r->phase==AP_COMMAND_RUNNING);r->phase=AP_COMMAND_DONE;published++;}
#include "executable-source.bpf.h"
static void *lookup(void *map,const void *key) {
    if(map==&ap_config_map && *(const u32 *)key==0)return missing_config?NULL:&config;
    if(map==&executable_sources && *(const u32 *)key==7)return missing_row?NULL:&row;
    assert(0);return NULL;
}
static long read_user(void *out,u32 size,const void *address) {
    assert(address==&iov && size==sizeof(iov));if(user_error)return -14;memcpy(out,address,size);return 0;
}
static long find_vma(struct task_struct *task,u64 address,void *callback,void *context,u64 flags) {
    assert(task==&target && address==row.intent.address && !flags);find_calls++;
    long (*run)(struct task_struct *,struct vm_area_struct *,struct ap_exe_vma_context *)=callback;
    if(!no_callback)run(task,&vma,context);
    if(double_callback)run(task,&vma,context);
    return helper_result;
}
static void reset(void) {
    missing_row=missing_result=missing_config=user_error=no_callback=double_callback=false;
    helper_result=0;published=find_calls=0;failures=0;
    sb=(struct super_block){AP_SOURCE_BTRFS_MAGIC,47};
    inode=(struct inode){.i_sb=&sb,.i_ino=53,.i_size=0x4000,.i_mode=0100755,.i_writecount={-1}};
    mapping=(struct address_space){&inode};
    file=(struct file){&inode,&mapping,(void *)AP_SOURCE_BTRFS_FOPS_IMAGE,1|32|AP_EXE_NONOTIFY};
    mm.exe_file=&file;tracer=(struct task_struct){.start_boottime=29};
    target=(struct task_struct){.mm=&mm,.start_boottime=19,.parent=&tracer,.ptrace=1,
        .__state=AP_TASK_TRACED,.jobctl=AP_JOBCTL_FROZEN|AP_JOBCTL_TRACED};
    vma=(struct vm_area_struct){.vm_mm=&mm,.vm_file=&file,.vm_ops=(void *)AP_EXE_VMOPS_IMAGE,
        .vm_start=0x401000,.vm_end=0x402000,.vm_pgoff=1,.vm_flags=5};
    iov=(struct iovec){(void *)0x701000,216};
    config=(struct ap_config){.provider=3,.anchor_phase=AP_GROUPED_ANCHOR_ACTIVE,.anchor_ip=AP_GROUPED_CONNECT_IMAGE};
    submitted=(struct ap_task_command){.provider=3,.command=7,.operation=26,.expected_object=11,
        .generation_before=13,.generation_after=0,.expected_level=AP_PTRACE_GETREGSET,.expected_option=1,.original_count=32};
    completion=(struct ap_command_result){.command=7,.operation=26,.identity={3,0,0},.phase=AP_COMMAND_READY,.original_count=32};
    row=(struct ap_executable_source){.intent={7,13,0,11,0x401040,32,(u64)&iov,0x701000}};
    exe_find_vma=find_vma;exe_read_user=read_user;
}
static void phase(bool exit,s64 raw) {
    u64 ctx[]={(u64)&target,AP_PTRACE_GETREGSET,AP_NT_PRSTATUS,(u64)&iov,(u64)raw};
    if(exit)executable_source_returned(ctx,&submitted);else executable_source_enter(ctx,&submitted);
}
#define CHECK(x) do {assert(x);checks++;} while(0)
#define VALID() ap_executable_source_matches(&submitted,&completion,&row,AP_GROUPED_CONNECT_IMAGE)
static void unrelated_ptrace_requests(void) {
    for(unsigned stage=0;stage<3;stage++) {
        reset();
        if(stage>=1)phase(false,0);
        if(stage>=2)phase(true,0);
        const struct ap_executable_source saved_row=row;
        const struct ap_command_result saved_completion=completion;
        const struct ap_task_command saved_submitted=submitted;
        const unsigned saved_published=published,saved_find_calls=find_calls;
        const u64 saved_failures=failures;
        for(unsigned other=0;other<2;other++) {
            /* The backend must perform its real XSTATE GET after PRSTATUS.
             * Neither that request nor another ptrace operation belongs to
             * this command, even before ENTERED or after DONE before collect. */
            u64 ctx[]={(u64)&target,other?AP_PTRACE_GETREGSET+1:AP_PTRACE_GETREGSET,
                other?AP_NT_PRSTATUS:0x202,0xdeadbeef,0};
            executable_source_enter(ctx,&submitted);
            CHECK(!memcmp(&row,&saved_row,sizeof(row)) &&
                !memcmp(&completion,&saved_completion,sizeof(completion)) &&
                !memcmp(&submitted,&saved_submitted,sizeof(submitted)) &&
                published==saved_published && find_calls==saved_find_calls && failures==saved_failures);
            executable_source_returned(ctx,&submitted);
            CHECK(!memcmp(&row,&saved_row,sizeof(row)) &&
                !memcmp(&completion,&saved_completion,sizeof(completion)) &&
                !memcmp(&submitted,&saved_submitted,sizeof(submitted)) &&
                published==saved_published && find_calls==saved_find_calls && failures==saved_failures);
        }
        if(stage==0)phase(false,0);
        if(stage<2)phase(true,0);
        CHECK(VALID() && published==1 && find_calls==2 && !failures);
        phase(false,0);
        CHECK(failures && published==1 && find_calls==2);
    }
    reset();
    u64 wrong_iov[]={(u64)&target,AP_PTRACE_GETREGSET,AP_NT_PRSTATUS,(u64)&iov+8,0};
    executable_source_enter(wrong_iov,&submitted);
    CHECK(row.problem && !published && !find_calls && !VALID());
    executable_source_returned(wrong_iov,&submitted);
    CHECK(row.problem && published==1 && !VALID());
}
static void callback_cardinality(void) {
    reset();phase(false,0);
    CHECK(!row.problem && row.phases==(AP_EXE_ENTERED|AP_EXE_OBSERVED));
    struct ap_exe_vma_context context={&row,&row.entered,config.anchor_ip,0};
    CHECK(exe_observe_vma(&target,&vma,&context)==0 && context.called==1 && !row.problem);
    const struct ap_executable_observation original=row.entered;
    /* Every repeated callback remains duplicate, including the verifier's
     * observed hundreds of visits. It cannot rewrite the first observation. */
    for(unsigned n=2;n<=1024;n++) {
        inode.i_size++;
        CHECK(exe_observe_vma(&target,&vma,&context)==0 && context.called==2 &&
            row.problem==AP_EXE_CONTEXT && !memcmp(&original,&row.entered,sizeof(original)));
    }
    reset();no_callback=true;phase(false,0);
    CHECK(row.problem==AP_EXE_HELPER && !VALID());
    reset();double_callback=true;phase(false,0);
    CHECK(row.problem==(AP_EXE_CONTEXT|AP_EXE_HELPER) && !VALID());
    phase(true,0);
    CHECK(row.problem==(AP_EXE_CONTEXT|AP_EXE_HELPER) && !VALID() && published==1);
}
int main(void) {
    reset();phase(false,0);CHECK(!VALID() && !published && row.phases==3);phase(true,0);
    CHECK(VALID() && published==1 && find_calls==2 && !failures);
    reset();phase(false,0);inode.i_writecount.counter=-7;phase(true,0);CHECK(VALID());
    for(unsigned test=0;test<27;test++) {
        reset();
        switch(test) {
        case 0:missing_row=true;break;case 1:missing_result=true;break;case 2:missing_config=true;break;
        case 3:user_error=true;break;case 4:no_callback=true;break;case 5:double_callback=true;break;
        case 6:helper_result=-16;break;case 7:target.parent=NULL;break;case 8:target.__state=0;break;
        case 9:target.jobctl=AP_JOBCTL_TRACED;break;case 10:target.ptrace=0;break;
        case 11:mm.exe_file=NULL;break;case 12:mapping.host=NULL;break;case 13:vma.vm_userfaultfd_ctx.ctx=&mm;break;
        case 14:file.f_mode|=2;break;case 15:inode.i_writecount.counter=0;break;case 16:vma.vm_flags|=8;break;
        case 17:vma.vm_file=NULL;break;case 18:vma.vm_ops=NULL;break;case 19:file.f_op=NULL;break;
        case 20:iov.iov_len=215;break;case 21:iov.iov_base=NULL;break;case 22:sb.s_magic=0;break;
        case 23:inode.i_size=1;break;case 24:row.intent.command++;break;case 25:completion.original_count++;break;
        case 26:row.phases=1;break;
        }
        phase(false,0);if(completion.phase==AP_COMMAND_RUNNING)phase(true,0);
        CHECK(!VALID());CHECK(failures || row.problem);
    }
    for(unsigned test=0;test<8;test++) {
        reset();phase(false,0);
        switch(test) {case 0:target.start_boottime++;break;case 1:tracer.start_boottime++;break;
        case 2:vma.vm_flags^=4;break;case 3:iov.iov_len=0;break;case 4:inode.i_writecount.counter=0;break;
        case 5:config.anchor_phase=0;break;case 6:row.intent.call++;break;case 7:user_error=true;break;}
        phase(true,0);CHECK(!VALID());CHECK(failures || row.problem);
    }
    reset();phase(true,0);CHECK(failures && !published && !VALID());
    reset();phase(false,0);phase(false,0);CHECK(failures && !published);
    reset();phase(false,0);phase(true,-14);CHECK(!VALID() && row.ptrace_return==-14 && completion.returned==-14);
    reset();phase(false,0);phase(true,0);phase(true,0);CHECK(failures && published==1);
    CHECK(checks==77); /* All original controls ran before the additive cases. */
    unrelated_ptrace_requests();
    CHECK(checks==98); /* All preexisting controls remain unchanged. */
    callback_cardinality();
    printf("executable actual producer: %u checks\n",checks);return 0;
}
