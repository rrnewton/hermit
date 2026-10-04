/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#include <assert.h>
#include <stdio.h>
#include <stddef.h>
#include "executable-source.h"
static struct ap_task_command command(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_EXECUTABLE_SOURCE,
        .expected_object=11,.generation_before=13,.generation_after=0,
        .expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS,.original_count=32};
}
static struct ap_command_result result(void) {
    return (struct ap_command_result){.command=7,.operation=AP_EXECUTABLE_SOURCE,.task=17,.start_boottime=19,
        .identity={3,0,0},.phase=AP_COMMAND_DONE,.original_count=32};
}
static struct ap_executable_source receipt(void) {
    struct ap_executable_source e={.intent={7,13,0,11,0x401040,32,0x700000,0x701000},
        .phases=AP_EXE_ENTERED|AP_EXE_OBSERVED|AP_EXE_RETURNED};
    e.entered=(struct ap_executable_observation){.task=17,.start=19,.tracer=23,.tracer_start=29,
        .mm=31,.file=37,.exe_file=37,.inode=41,.mapping=43,
        .fops=AP_SOURCE_BTRFS_FOPS_IMAGE,.vm_ops=AP_EXE_VMOPS_IMAGE,.filesystem=AP_SOURCE_BTRFS_MAGIC,
        .device=47,.inode_number=53,.file_size=0x4000,.vm_start=0x401000,.vm_end=0x402000,
        .vm_pgoff=1,.vm_flags=5,.file_mode=AP_EXE_FMODE_REQUIRED|AP_EXE_NONOTIFY,.inode_mode=0100755,.writecount=-1,
        .iovec_base=0x701000,.iovec_length=AP_EXE_REGISTER_BYTES};
    e.returned=e.entered;return e;
}
static struct ap_executable_source retry22_receipt(void) {
    /* Actual retry22 read-only PROGRESS mapping; command/task identities remain
     * the controlled fixture's. Linux f_mode has no __FMODE_EXEC open flag. */
    struct ap_executable_source e=receipt();
    e.intent.address=0x403029;e.intent.length=5;
    e.entered.file_size=37408;e.entered.vm_start=0x403000;e.entered.vm_end=0x404000;
    e.entered.vm_pgoff=3;e.entered.vm_flags=0x71;e.entered.file_mode=0x0c4a801d;
    e.returned=e.entered;return e;
}
int main(void) {
    unsigned checks=0;struct ap_task_command c=command();struct ap_command_result r=result();
    struct ap_executable_source e=receipt();u64 anchor=AP_GROUPED_CONNECT_IMAGE;
#define CHECK(x) do {assert(x);checks++;} while(0)
#define VALID() ap_executable_source_matches(&c,&r,&e,anchor)
    CHECK(sizeof(e)==472 && offsetof(struct ap_executable_source,returned)==248 &&
        offsetof(struct ap_executable_source,phases)==432);
    CHECK(VALID()); /* Actual initial semantic MM zero is legitimate. */
    e.returned.writecount=-2;CHECK(VALID());e=receipt();
    e.entered.file_mode=e.returned.file_mode=AP_EXE_FMODE_REQUIRED|AP_EXE_NONOTIFY_PERM;CHECK(VALID());e=receipt();
    anchor-=0x200000;e.entered.fops=e.returned.fops-=0x200000;
    e.entered.vm_ops=e.returned.vm_ops-=0x200000;CHECK(VALID());e=receipt();anchor=AP_GROUPED_CONNECT_IMAGE;
#define BAD_C(field,value) do {c=command();c.field=value;CHECK(!VALID());c=command();} while(0)
    BAD_C(provider,4);BAD_C(command,8);BAD_C(operation,25);BAD_C(expected_object,12);
    BAD_C(generation_before,0);BAD_C(generation_after,1);BAD_C(expected_level,0);BAD_C(expected_option,2);
    BAD_C(original_count,31);BAD_C(expected_timeout_ticks,1);
#define BAD_R(field,value) do {r=result();r.field=value;CHECK(!VALID());r=result();} while(0)
    BAD_R(command,8);BAD_R(operation,25);BAD_R(task,18);BAD_R(start_boottime,20);
    BAD_R(identity.provider,4);BAD_R(identity.object,1);BAD_R(identity.namespace,1);BAD_R(creation,1);
    BAD_R(cookie,1);BAD_R(returned,-14);BAD_R(reserved,1);BAD_R(phase,AP_COMMAND_RUNNING);
    BAD_R(original_count,31);BAD_R(state.lowat,1);BAD_R(state.tcp_state,1);
#define BAD_E(field,value) do {e=receipt();e.field=value;CHECK(!VALID());e=receipt();} while(0)
    BAD_E(intent.command,8);BAD_E(intent.registration,0);BAD_E(intent.owner_mm,1);BAD_E(intent.call,12);
    BAD_E(intent.address,0);BAD_E(intent.address,0x401ff0);BAD_E(intent.address,~0ULL-15);
    BAD_E(intent.length,0);BAD_E(intent.length,513);BAD_E(intent.iovec,0);
    BAD_E(intent.registers,0);BAD_E(intent.iovec,0x701000);
    BAD_E(phases,0);BAD_E(phases,3);BAD_E(phases,15);BAD_E(problem,1);
    BAD_E(ptrace_return,-14);BAD_E(find_enter_return,-16);BAD_E(find_exit_return,-2);
#define BAD_OBS(field,value) do {e=receipt();e.entered.field=e.returned.field=value;CHECK(!VALID());e=receipt();} while(0)
    BAD_OBS(task,0);BAD_OBS(start,0);BAD_OBS(tracer,0);BAD_OBS(tracer_start,0);BAD_OBS(mm,0);
    BAD_OBS(file,0);BAD_OBS(exe_file,38);BAD_OBS(inode,0);BAD_OBS(mapping,0);
    BAD_OBS(fops,AP_SOURCE_BTRFS_FOPS_IMAGE+8);BAD_OBS(vm_ops,AP_EXE_VMOPS_IMAGE+8);
    BAD_OBS(filesystem,0);BAD_OBS(inode_number,0);BAD_OBS(inode_mode,0040755);
    BAD_OBS(inode_mode,0x10000);BAD_OBS(file_size,0);BAD_OBS(file_size,~0ULL);
    BAD_OBS(file_size,0x1050);BAD_OBS(vm_start,0);BAD_OBS(vm_start,0x401001);
    BAD_OBS(vm_end,0x401000);BAD_OBS(vm_end,0x402001);BAD_OBS(vm_pgoff,~0ULL);
    BAD_OBS(vm_flags,0);BAD_OBS(vm_flags,7);BAD_OBS(vm_flags,0xd);
    for(unsigned bit=0;bit<64;bit++)if(AP_EXE_VM_FORBIDDEN&(1ULL<<bit)) {
        e=receipt();e.entered.vm_flags=e.returned.vm_flags=1|(1ULL<<bit);CHECK(!VALID());
    }
    e=receipt();
    BAD_OBS(file_mode,AP_EXE_FMODE_REQUIRED);
    BAD_OBS(file_mode,AP_EXE_FMODE_REQUIRED|AP_EXE_NONOTIFY|AP_EXE_NONOTIFY_PERM);
    BAD_OBS(file_mode,AP_EXE_FMODE_REQUIRED|AP_EXE_FMODE_WRITE|AP_EXE_NONOTIFY);
    /* The former missing-FMODE_EXEC negative asserted an open-flag property
     * that Linux does not store in f_mode. This incomplete open remains invalid
     * because OPENED and CAN_READ are absent; test each real bit below. */
    BAD_OBS(file_mode,AP_EXE_FMODE_READ|AP_EXE_NONOTIFY);
    BAD_OBS(writecount,0);BAD_OBS(writecount,1);BAD_OBS(writecount,-2147483649LL);
    BAD_OBS(iovec_base,0x701001);BAD_OBS(iovec_length,215);BAD_OBS(iovec_length,217);
    /* Every immutable identity/geometry word differs at only the return cut.
     * Writecount is intentionally excluded and independently checked above. */
#define CHANGED(field) do {e=receipt();e.returned.field++;CHECK(!VALID());} while(0)
    CHANGED(task);CHANGED(start);CHANGED(tracer);CHANGED(tracer_start);CHANGED(mm);CHANGED(file);
    CHANGED(exe_file);CHANGED(inode);CHANGED(mapping);CHANGED(fops);CHANGED(vm_ops);CHANGED(filesystem);
    CHANGED(device);CHANGED(inode_number);CHANGED(file_size);CHANGED(vm_start);CHANGED(vm_end);
    CHANGED(vm_pgoff);CHANGED(vm_flags);CHANGED(file_mode);CHANGED(inode_mode);CHANGED(iovec_base);CHANGED(iovec_length);
    e=receipt();CHECK(!ap_executable_source_matches(&c,&r,&e,0));
    for(u64 anchor=0;anchor<3;anchor++) {
        e=receipt();e.entered.fops=e.returned.fops=0;e.entered.vm_ops=e.returned.vm_ops=0;
        CHECK(!ap_executable_source_matches(&c,&r,&e,anchor));
    }
    CHECK(ap_executable_range(0x401000,512));CHECK(ap_executable_range(0x401fff,1));
    CHECK(!ap_executable_range(0x401fff,2));CHECK(!ap_executable_range(0x800000000000ULL,1));
    c=command();r=result();c.original_count=r.original_count=5;
    e=retry22_receipt();CHECK(VALID());
    for(unsigned cut=0;cut<2;cut++)for(unsigned test=0;test<15;test++) {
        e=retry22_receipt();
        struct ap_executable_observation *o=cut?&e.returned:&e.entered;
        switch(test) {
        case 0:o->file_mode&=~AP_EXE_FMODE_READ;break;
        case 1:o->file_mode&=~AP_EXE_FMODE_OPENED;break;
        case 2:o->file_mode&=~AP_EXE_FMODE_CAN_READ;break;
        case 3:o->file_mode|=AP_EXE_FMODE_WRITE;break;
        case 4:o->exe_file++;break;
        case 5:o->writecount=0;break;
        case 6:o->file_mode&=~(AP_EXE_NONOTIFY|AP_EXE_NONOTIFY_PERM);break;
        case 7:o->file_mode|=AP_EXE_NONOTIFY|AP_EXE_NONOTIFY_PERM;break;
        case 8:o->fops++;break;
        case 9:o->vm_ops++;break;
        case 10:o->vm_flags|=2;break;
        case 11:o->vm_flags|=8;break;
        case 12:o->file_size=0x302d;break;
        case 13:o->vm_end=o->vm_start;break;
        /* The old synthetic EXEC flag cannot replace persistent open state. */
        case 14:o->file_mode=1|32|AP_EXE_NONOTIFY;break;
        }
        CHECK(!ap_executable_observation_valid(&e.intent,o,anchor));
        CHECK(!VALID());
    }
    printf("executable source predicate controls: %u passed\n",checks);return 0;
}
