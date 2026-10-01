/* SPDX-License-Identifier: MIT */
/* Test-only boundary driver. All records below are emitted by the same
 * production bodies as BPF and checked by the actual C collector. Selection,
 * helper callbacks, saved kernel frame and protocol/sys_exit are modeled.
 * No kernel entry authentication, source ownership or ACK is claimed. */
#define main producer_boundary_selftest
#include "stream-copy-fault-producer-test.c"
#undef main

int main(int argc,char **argv) {
    /* The exact historical exit-body cohort intentionally does not call this
     * newer issuer-consumer. Retain its declaration without disabling Werror;
     * taking its address supplies no copied count or witness consumption. */
    (void)stream_copy_fault_complete;
    assert(argc==2);
    const bool full=!strcmp(argv[1],"full");
    const bool prior=!strcmp(argv[1],"prior");
    const bool two=!strcmp(argv[1],"two");
    assert(full || prior || two || !strcmp(argv[1],"fault"));
    setup(512,two?2:1);
    /* Explicit selection-boundary premise required by the unchanged Rust
     * Controller binder. The producer callback fixture does not issue it. */
    active.original.selection.table=19;
    if(prior) {
        active.original.stream_copy.summary.initial_count=1024;
        active.original.selection.original_count=1024;iter.count=1024;
        helper_enter(512);iter.count=512;iter.iov_offset=512;helper_exit(0);
        assert(!problems);((struct tcp_skb_cb *)skb.cb)->seq=554;
    }
    helper_enter(512);
    if(full) {iter.count=0;iter.iov_offset=512;}
    else {
        if(two) {iter.count=384;iter.iov_offset=128;}
        struct pt_regs frame=terminal_frame(two?TEST_DIRECT+4096+200:TEST_DIRECT+100,
            two?384:512,two?321:449);
        terminal_pair(&frame);
        iter.count=512;iter.iov_offset=prior?512:0;
    }
    helper_exit(full?0:-14);assert(!problems);
    const s64 returned=full || prior?512:-14;
    active.original.stream_copy.summary.final_count=full?0:512;
    active.original.stream_copy.summary.protocol_returned=(u64)returned;
    active.original.stream_copy.summary.protocol_complete=1;
    active.original.stream_copy.iterator=0;active.original.stream_copy.copy_active=0;
    stream_copy_commit(&active,returned);assert(!problems);
    struct ap_session s={.incarnation=3,.ready=true};
    struct ap_pending_command *pending=&s.pending[ap_command_slot(7)];
    pending->state=AP_SLOT_ACTIVE;
    pending->submitted=(struct ap_task_command){.command=7,.operation=AP_ORIGINAL_READ,
        .expected_object=11,.original_count=prior?1024:512};
    for(u32 i=0;i<emitted_count;i++)
        assert(!stream_copy_record(&s,&emitted[i],sizeof(emitted[i])));
    assert(!s.stream_copy_error && pending->stream_copy.committed);
    /* No copied-count oracle here: the unchanged Rust oracle below must see
     * the actual records, including the historical producer's missing DATA. */
    assert(fwrite(emitted,sizeof(emitted[0]),emitted_count,stdout)==emitted_count);
    assert(!fflush(stdout));free(pending->stream_copy.records);return 0;
}
