/* SPDX-License-Identifier: BSD-3-Clause */
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/user.h>
#include "current-close-profile.h"
static struct ap_task_command command(void) {
    return (struct ap_task_command){.provider=3,.command=7,.operation=AP_CURRENT_CLOSE_PROFILE,
        .expected_object=11,.generation_before=13,.generation_after=0,
        .expected_level=AP_PTRACE_GETREGSET,.expected_option=AP_NT_PRSTATUS};
}
static struct ap_command_result result(void) {
    return (struct ap_command_result){.command=7,.operation=AP_CURRENT_CLOSE_PROFILE,
        .task=17,.start_boottime=19,.identity={3,0,0},.phase=AP_COMMAND_DONE};
}
static struct ap_close_profile receipt(void) {
    struct ap_close_profile e={.intent={7,13,0,0,11,12,4,0,3},.phases=7,
        .iovec=0x700000,.registers=0x701000,.register_bytes=216,.original_nr=3,.original_fd=4};
    e.entered=(struct ap_close_profile_observation){.task=17,.start=19,.tracer=23,.tracer_start=29,
        .mm=31,.table=11,.file=12,.raw_table=37,.raw_file=41,.socket=43,.sk=47,.inode=53,
        .file_ops=AP_CLOSE_SOCKET_FILE_OPS_IMAGE,.file_release=AP_CLOSE_SOCK_CLOSE_IMAGE,
        .socket_ops=AP_CLOSE_INET_STREAM_OPS_IMAGE,.socket_release=AP_CLOSE_INET_RELEASE_IMAGE,
        .protocol_ops=AP_CLOSE_TCP_PROT_IMAGE,.protocol_close=AP_CLOSE_TCP_CLOSE_IMAGE,
        .file_ref_raw=1,.file_refs=2,.files_refs=1,.max_fds=64,.aliases=2,.family=2,.type=1,.protocol=6};
    e.returned=e.entered;return e;
}
int main(void) {
    unsigned checks=0;struct ap_task_command c=command();struct ap_command_result r=result();
    struct ap_close_profile e=receipt();u64 anchor=AP_GROUPED_CONNECT_IMAGE;
#define CHECK(x) do {assert(x);checks++;} while(0)
#define SETTLED() ap_close_profile_matches(&c,&r,&e)
#define VALID() ap_close_profile_finite(&c,&r,&e,anchor)
    CHECK(sizeof(e)==576 && sizeof(e.entered)==224 && offsetof(struct ap_close_profile,returned)==288);
    CHECK(sizeof(struct user_regs_struct)==AP_CLOSE_REGISTER_BYTES);
    CHECK(offsetof(struct user_regs_struct,orig_rax)==15*8 && offsetof(struct user_regs_struct,rdi)==14*8);
    CHECK(VALID()); /* Semantic MM and Normal epoch zero are valid. */
    e.entered.file_ref_raw=e.returned.file_ref_raw=0;e.entered.file_refs=e.returned.file_refs=1;
    e.entered.aliases=e.returned.aliases=1;CHECK(VALID());e=receipt();
    e.entered.linger_ticks=100;e.returned.linger_ticks=300;CHECK(VALID());e=receipt();
    e.problem=AP_CLOSE_UNSUPPORTED;CHECK(SETTLED() && !VALID());e=receipt();
#define BAD_C(field,value) do {c=command();c.field=value;CHECK(!VALID());c=command();} while(0)
    BAD_C(provider,4);BAD_C(command,8);BAD_C(operation,26);BAD_C(expected_object,12);
    BAD_C(generation_before,0);BAD_C(generation_after,1);BAD_C(expected_level,0);BAD_C(expected_option,2);
    BAD_C(original_count,1);BAD_C(expected_timeout_ticks,1);
#define BAD_E(field,value) do {e=receipt();e.field=value;CHECK(!VALID());e=receipt();} while(0)
    BAD_E(intent.command,8);BAD_E(intent.registration,14);BAD_E(intent.owner_mm,1);
    BAD_E(intent.expected_file,13);BAD_E(intent.fd,-1);BAD_E(intent.fd,256);BAD_E(intent.reserved,1);
    BAD_E(phases,3);BAD_E(phases,15);BAD_E(problem,AP_CLOSE_READ);BAD_E(ptrace_return,-14);
    BAD_E(register_bytes,215);BAD_E(original_nr,45);BAD_E(original_fd,3);BAD_E(iovec,0);BAD_E(registers,0);
#define BAD_O(field,value) do {e=receipt();e.entered.field=e.returned.field=value;CHECK(!VALID());e=receipt();} while(0)
    BAD_O(task,0);BAD_O(start,0);BAD_O(tracer,0);BAD_O(tracer_start,0);BAD_O(mm,0);
    BAD_O(table,37);BAD_O(file,41);BAD_O(raw_table,0);BAD_O(raw_file,0);BAD_O(inode,0);
    BAD_O(files_refs,2);BAD_O(max_fds,257);BAD_O(aliases,0);BAD_O(file_ref_raw,2);BAD_O(file_refs,3);
    BAD_O(file_ref_raw,~0ULL);BAD_O(file_ref_raw,0x8000000000000000ULL);
    BAD_O(socket,0);BAD_O(sk,0);BAD_O(family,10);BAD_O(type,2);BAD_O(protocol,17);
    BAD_O(linger,1);BAD_O(repair,1);BAD_O(ulp_ops,1);BAD_O(ulp_data,1);BAD_O(file_flush,1);
    BAD_O(file_ops,AP_CLOSE_SOCKET_FILE_OPS_IMAGE+8);BAD_O(file_release,AP_CLOSE_SOCK_CLOSE_IMAGE+8);
    BAD_O(socket_ops,AP_CLOSE_INET_STREAM_OPS_IMAGE+8);BAD_O(socket_release,AP_CLOSE_INET_RELEASE_IMAGE+8);
    BAD_O(protocol_ops,AP_CLOSE_TCP_PROT_IMAGE+8);BAD_O(protocol_close,AP_CLOSE_TCP_CLOSE_IMAGE+8);
#define CHANGED(field) do {e=receipt();e.returned.field++;CHECK(!VALID());e=receipt();} while(0)
    CHANGED(task);CHANGED(start);CHANGED(tracer);CHANGED(tracer_start);CHANGED(mm);CHANGED(table);CHANGED(file);
    CHANGED(raw_table);CHANGED(raw_file);CHANGED(inode);CHANGED(aliases);CHANGED(file_ref_raw);CHANGED(file_refs);
    CHANGED(socket);CHANGED(sk);CHECK(!ap_close_profile_finite(&c,&r,&e,0));
    anchor-=0x200000;
    e.entered.file_ops=e.returned.file_ops-=0x200000;e.entered.file_release=e.returned.file_release-=0x200000;
    e.entered.socket_ops=e.returned.socket_ops-=0x200000;e.entered.socket_release=e.returned.socket_release-=0x200000;
    e.entered.protocol_ops=e.returned.protocol_ops-=0x200000;e.entered.protocol_close=e.returned.protocol_close-=0x200000;
    CHECK(VALID());printf("current Close predicate controls: %u passed\n",checks);return 0;
}
