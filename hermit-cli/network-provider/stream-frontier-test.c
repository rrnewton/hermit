/* SPDX-License-Identifier: MIT */
/* Production transition controls; no native/BPF effects or alternate model. */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "stream-frontier.h"

/* Independent field and byte population: every stale bit must keep the row
 * unenrolled and poison it, while preserving the complete stale layout. */
static void enrollment_layout_zero_controls(void) {
    _Static_assert(sizeof(struct ap_stream_copy_begin)==104,"copy5 Begin ABI");
    _Static_assert(__builtin_offsetof(struct ap_stream_copy_begin,disposition)==96,"last word ABI");
    const struct ap_stream_copy_begin zero={0};
    struct ap_fd_file file={.identity=7};
    assert(ap_stream_frontier_enroll_install(&file,7,19,23));
    assert(file.stream_state==AP_STREAM_FRONTIER_ENROLLED);
    assert(!memcmp(&file.stream_layout,&zero,sizeof(zero)));
    for(unsigned field=0;field<13;field++) {
        file=(struct ap_fd_file){.identity=7};
        switch(field) {
        case 0:file.stream_layout.file=1;break;
        case 1:file.stream_layout.before=1;break;
        case 2:file.stream_layout.start=1;break;
        case 3:file.stream_layout.order=1;break;
        case 4:file.stream_layout.offset=1;break;
        case 5:file.stream_layout.requested=1;break;
        case 6:file.stream_layout.available=1;break;
        case 7:file.stream_layout.source_offset=1;break;
        case 8:file.stream_layout.skb_length=1;break;
        case 9:file.stream_layout.nonlinear=1;break;
        case 10:file.stream_layout.position=1;break;
        case 11:file.stream_layout.transport=1;break;
        case 12:file.stream_layout.disposition=1;break;
        default:assert(0);
        }
        const struct ap_stream_copy_begin original=file.stream_layout;
        assert(!ap_stream_frontier_enroll_install(&file,7,19,23));
        assert(file.stream_state==AP_STREAM_FRONTIER_POISON);
        assert(!file.stream_birth_command && !file.stream_birth_install);
        assert(!memcmp(&file.stream_layout,&original,sizeof(original)));
    }
    for(unsigned byte=0;byte<sizeof(zero);byte++)for(unsigned bit=0;bit<8;bit++) {
        file=(struct ap_fd_file){.identity=7};
        ((unsigned char *)&file.stream_layout)[byte]=(unsigned char)(1U<<bit);
        const struct ap_stream_copy_begin original=file.stream_layout;
        assert(!ap_stream_frontier_enroll_install(&file,7,19,23));
        assert(file.stream_state==AP_STREAM_FRONTIER_POISON);
        assert(!file.stream_birth_command && !file.stream_birth_install);
        assert(!memcmp(&file.stream_layout,&original,sizeof(original)));
    }
    puts("frontier enrollment: zero positive,13 independent fields,832 individual bit refusals");
}
static struct ap_fd_file fresh(u64 identity) {
    struct ap_fd_file file={.identity=identity};
    assert(ap_stream_frontier_enroll_install(&file,identity,19,23));
    assert(file.stream_bytes==0 && file.stream_units==0);
    return file;
}
static struct ap_stream_frontier_request request(u64 command,u64 attempt,u64 offset,
        u64 requested,u64 available,u64 disposition) {
    return (struct ap_stream_frontier_request){.file=7,.command=command,.attempt=attempt,
        .disposition=disposition,.iterator_offset=offset,.requested=requested,.available=available};
}
static struct ap_stream_frontier_end complete(struct ap_fd_file *file,
        const struct ap_stream_frontier_request *r,u64 start,u64 copied,s64 result) {
    struct ap_stream_frontier_begin begin={0};struct ap_stream_frontier_end end={0};
    assert(ap_stream_frontier_begin_attempt(file,r,&begin));
    assert(begin.before==file->stream_bytes && begin.order==file->stream_units);
    assert(begin.start==start);
    assert(ap_stream_frontier_end_attempt(file,r,copied,result,&end));
    assert(end.before==begin.before && end.order_before==begin.order);
    assert(end.after==file->stream_bytes && end.order_after==file->stream_units);
    assert(ap_stream_frontier_state(file)==AP_STREAM_FRONTIER_ENROLLED);
    assert(!file->stream_command && !file->stream_attempt &&
        !file->stream_entry_bytes && !file->stream_entry_order && !file->stream_requested &&
        !file->stream_iterator_offset && !file->stream_available);
    return end;
}
static void interleaved_consumers(void) {
    struct ap_fd_file file=fresh(7);
    const struct ap_stream_frontier_request a1=request(31,1,0,2,8,AP_STREAM_COPY_CONSUME),
        b1=request(32,1,0,2,6,AP_STREAM_COPY_CONSUME),
        a2=request(31,2,2,2,4,AP_STREAM_COPY_CONSUME);
    assert(complete(&file,&a1,0,2,0).after==2);
    assert(complete(&file,&b1,2,2,0).after==4);
    /* A's local output offset is2, but its next native byte start is4, not6. */
    assert(complete(&file,&a2,4,2,0).after==6);
    assert(file.stream_units==3);
}
static void extent_peek_fault_retry(void) {
    struct ap_fd_file file=fresh(7);
    struct ap_stream_frontier_request r=request(31,1,0,32,64,AP_STREAM_COPY_OBSERVE);
    assert(complete(&file,&r,0,32,0).after==0);assert(file.stream_units==0);
    r=request(31,2,32,32,32,AP_STREAM_COPY_OBSERVE);
    assert(complete(&file,&r,32,32,0).after==0);assert(file.stream_units==0);
    r=request(32,1,0,64,64,AP_STREAM_COPY_CONSUME);
    assert(complete(&file,&r,0,32,-14).after==0);assert(file.stream_units==0);
    r=request(33,1,0,64,64,AP_STREAM_COPY_CONSUME);
    assert(complete(&file,&r,0,64,0).after==64);assert(file.stream_units==1);
    r=request(33,2,64,8,8,AP_STREAM_COPY_CONSUME);
    assert(complete(&file,&r,64,3,-14).after==64);assert(file.stream_units==1);
    /* Unix positions may restart0 on the next object; byte frontier does not. */
    r=request(34,1,0,8,8,AP_STREAM_COPY_CONSUME);
    assert(complete(&file,&r,64,8,0).after==72);assert(file.stream_units==2);
}
static void unenrolled_and_incarnation_refusals(void) {
    struct ap_fd_file file={.identity=7};
    assert(!ap_stream_frontier_member(&file,7));
    assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
    assert(!ap_stream_frontier_enroll_install(&file,7,19,23));
    assert(!file.stream_bytes && !file.stream_units);
    file=fresh(7);assert(!ap_stream_frontier_enroll_install(&file,7,19,23));
    assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
    file=fresh(8);assert(!ap_stream_frontier_member(&file,7));
    assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
    /* A removed/reused physical address has a new row, not old byte authority. */
    file=(struct ap_fd_file){.identity=8};
    assert(!ap_stream_frontier_member(&file,8));
    assert(!ap_stream_frontier_enroll_install(&file,8,29,33));
}
static void active_poison_and_identity_refusals(void) {
    for(unsigned mutation=0;mutation<10;mutation++) {
        struct ap_fd_file file=fresh(7);
        struct ap_stream_frontier_request r=request(31,1,0,4,4,AP_STREAM_COPY_CONSUME);
        struct ap_stream_frontier_begin begin={0};
        assert(ap_stream_frontier_begin_attempt(&file,&r,&begin));
        struct ap_stream_frontier_request changed=r;
        switch(mutation) {
        case 0:ap_stream_frontier_poison(&file);break;
        case 1:changed.file++;break;
        case 2:changed.command++;break;
        case 3:changed.attempt++;break;
        case 4:changed.disposition=AP_STREAM_COPY_OBSERVE;break;
        case 5:file.stream_bytes++;break;
        case 6:file.stream_units++;break;
        case 7:changed.requested++;break;
        case 8:changed.iterator_offset++;break;
        case 9:changed.available++;break;
        }
        const u64 bytes=file.stream_bytes,order=file.stream_units;
        struct ap_stream_frontier_end end={11,12,13,14},unchanged=end;
        assert(!ap_stream_frontier_end_attempt(&file,&changed,4,0,&end));
        assert(!memcmp(&end,&unchanged,sizeof(end)));
        assert(file.stream_bytes==bytes && file.stream_units==order);
        assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
        assert(!ap_stream_frontier_end_attempt(&file,&r,4,0,&end));
        assert(!ap_stream_frontier_enroll_install(&file,7,19,23));
        assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
    }
    struct ap_fd_file file=fresh(7);
    struct ap_stream_frontier_request r=request(31,1,0,4,4,AP_STREAM_COPY_CONSUME);
    struct ap_stream_frontier_begin begin={0};
    assert(ap_stream_frontier_begin_attempt(&file,&r,&begin));
    assert(!ap_stream_frontier_begin_attempt(&file,&r,&begin));
    assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
}
static void protocol_entry_before_receive_lock_does_not_poison_an_active_reader(void) {
    for(unsigned observe=0;observe<2;observe++) {
        struct ap_fd_file file=fresh(7);
        struct ap_stream_frontier_request r=request(31,1,0,4,4,
            observe?AP_STREAM_COPY_OBSERVE:AP_STREAM_COPY_CONSUME);
        struct ap_stream_frontier_begin begin={0};struct ap_stream_frontier_end end={0};
        assert(ap_stream_frontier_begin_attempt(&file,&r,&begin));
        const struct ap_fd_file active=file;
        assert(ap_stream_frontier_member(&file,7));
        assert(!memcmp(&file,&active,sizeof(file)));
        assert(ap_stream_frontier_end_attempt(&file,&r,4,0,&end));
        assert(end.after==(observe?0:4));
        assert(ap_stream_frontier_member(&file,7));
    }
    struct ap_fd_file file=fresh(7);
    file.stream_state|=AP_STREAM_FRONTIER_OBSERVING; /* no paired active attempt */
    assert(!ap_stream_frontier_member(&file,7));
    assert(file.stream_state&AP_STREAM_FRONTIER_POISON);
}
static void bounds_are_transactional(void) {
    for(unsigned mutation=0;mutation<5;mutation++) {
        struct ap_fd_file file=fresh(7);
        struct ap_stream_frontier_request r=request(31,1,0,4,4,AP_STREAM_COPY_CONSUME);
        if(mutation==0)r.requested=5;
        if(mutation==1) {file.stream_bytes=~0ULL-3;r.available=4;}
        if(mutation==2) {file.stream_bytes=~0ULL-4;r.iterator_offset=1;r.disposition=AP_STREAM_COPY_OBSERVE;}
        if(mutation==3)r.available=0;
        if(mutation==4)r.disposition=3;
        const u64 bytes=file.stream_bytes,order=file.stream_units;
        struct ap_stream_frontier_begin begin={11,12,13},unchanged=begin;
        assert(!ap_stream_frontier_begin_attempt(&file,&r,&begin));
        assert(!memcmp(&begin,&unchanged,sizeof(begin)));
        assert(file.stream_bytes==bytes && file.stream_units==order);
        assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
    }
    struct ap_fd_file file=fresh(7);file.stream_units=~0ULL;
    struct ap_stream_frontier_request r=request(31,1,0,4,4,AP_STREAM_COPY_CONSUME);
    struct ap_stream_frontier_begin begin={0};struct ap_stream_frontier_end end={11,12,13,14},unchanged=end;
    assert(ap_stream_frontier_begin_attempt(&file,&r,&begin));
    assert(!ap_stream_frontier_end_attempt(&file,&r,4,0,&end));
    assert(file.stream_units==~0ULL && file.stream_bytes==0);
    assert(!memcmp(&end,&unchanged,sizeof(end)));
    assert(ap_stream_frontier_state(&file)&AP_STREAM_FRONTIER_POISON);
}
static void actual_frame_stage_and_protocol_domain(void) {
    for(u64 protocol=1;protocol<=3;protocol++) {
        const u64 transport=protocol==3?AP_STREAM_COPY_UNIX:AP_STREAM_COPY_TCP;
        u64 word=ap_stream_frame_begin(protocol,0x1000);
        assert(!ap_stream_frontier_frame_valid(ap_stream_frame_protocol(word),ap_stream_frame_stage(word),transport));
        if(protocol!=3) {
            word=ap_stream_frame_advance(word,protocol,2);
            assert(!ap_stream_frontier_frame_valid(ap_stream_frame_protocol(word),ap_stream_frame_stage(word),transport));
        }
        word=ap_stream_frame_advance(word,protocol,3);
        assert(ap_stream_frontier_frame_valid(ap_stream_frame_protocol(word),ap_stream_frame_stage(word),transport));
        assert(!ap_stream_frontier_frame_valid(ap_stream_frame_protocol(word),ap_stream_frame_stage(word),3-transport));
        assert(!ap_stream_frontier_frame_valid(0,3,transport));
        assert(!ap_stream_frontier_frame_valid(protocol,4,transport));
    }
    assert(!ap_stream_frontier_frame_valid(ap_stream_frame_protocol(0),ap_stream_frame_stage(0),AP_STREAM_COPY_TCP));
}
int main(void) {
    enrollment_layout_zero_controls();
    actual_frame_stage_and_protocol_domain();
    interleaved_consumers();extent_peek_fault_retry();unenrolled_and_incarnation_refusals();
    active_poison_and_identity_refusals();bounds_are_transactional();
    protocol_entry_before_receive_lock_does_not_poison_an_active_reader();
    puts("stream-frontier controls passed: actual transition header; no native qualification");
    return 0;
}
