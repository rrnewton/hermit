/* SPDX-License-Identifier: MIT */
/* Host controls of the actual production interval/storage decoder and ring
 * collector. They do NOT assert real kernel hook execution or page lifetime. */
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include "fd-effects.h"
static unsigned checks;
#define TX_CHECK(x) do {assert(x);checks++;} while(0)
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
static void image_controls(void) {
    const u8 prefix[]={0x55,0x41,0x57,0x41,0x56,0x41,0x55,0x41,0x54,0x53,
        0x48,0x81,0xec,0xa8,0,0,0,0x49,0x89,0xd4,0x49,0x89,0xf7,0x48,0x89,0xfb,
        0x31,0xed,0x31,0xf6,0xe8,0xa8,0xc4,0xff,0xff};
    const u8 suffix[]={0x48,0x89,0xdf,0xe8,0xd3,0x93,0xf9,0xff,0x89,0xe8,
        0x48,0x81,0xc4,0xa8,0,0,0,0x5b,0x41,0x5c,0x41,0x5d,0x41,0x5e,0x41,0x5f,0x5d,0xc3};
    u64 a[5]={0},b[4]={0};
    memcpy(a,prefix,sizeof(prefix));memcpy(b,suffix,sizeof(suffix));
    TX_CHECK(ap_stream_tx_image_words(a,b));
    for(size_t i=0;i<sizeof(prefix);i++) {
        ((u8 *)a)[i]^=1;TX_CHECK(!ap_stream_tx_image_words(a,b));((u8 *)a)[i]^=1;
    }
    for(size_t i=0;i<sizeof(suffix);i++) {
        ((u8 *)b)[i]^=1;TX_CHECK(!ap_stream_tx_image_words(a,b));((u8 *)b)[i]^=1;
    }
}
static int blocking_window(unsigned index,const u64 *words) {
    switch(index) {
    case 0:return ap_tx_prefix_words(words);
    case 1:return ap_tx_load_words(words);
    case 2:return ap_tx_wait_memory_words(words);
    case 3:return ap_tx_wait_connect_words(words);
    default:return 0;
    }
}
static void blocking_image_controls(void) {
    const unsigned offsets[]={AP_TX_PREFIX_OFFSET,AP_TX_LOAD_OFFSET,AP_TX_WAIT_MEMORY_OFFSET,AP_TX_WAIT_CONNECT_OFFSET};
    const unsigned lengths[]={AP_TX_PREFIX_SIZE,AP_TX_LOAD_SIZE,AP_TX_WAIT_MEMORY_SIZE,AP_TX_WAIT_CONNECT_SIZE};
    for(unsigned w=0;w<4;w++) {
        u64 words[29]={0};memcpy(words,ap_tx_image_tcp_sendmsg+offsets[w],lengths[w]);
        TX_CHECK(blocking_window(w,words));
        for(unsigned i=0;i<lengths[w];i++) {
            ((u8 *)words)[i]^=1;TX_CHECK(!blocking_window(w,words));((u8 *)words)[i]^=1;
        }
    }
}
static void interval_controls(void) {
    TX_CHECK(ap_stream_tx_caller(0x1000,0x10000,0xff20,0x1028,AP_STREAM_TX_LOCK_RETURN));
    TX_CHECK(ap_stream_tx_caller(0x1000,0x10000,0xff20,0x185d,AP_STREAM_TX_UNLOCK_RETURN));
    TX_CHECK(!ap_stream_tx_caller(0x1000,0x10000,0xff28,0x1028,AP_STREAM_TX_LOCK_RETURN));
    TX_CHECK(!ap_stream_tx_caller(0x1000,0x10000,0xff20,0x1029,AP_STREAM_TX_LOCK_RETURN));
    TX_CHECK(!ap_stream_tx_caller(~0ULL-8,0x10000,0xff20,0x1028,AP_STREAM_TX_LOCK_RETURN));
    TX_CHECK(!ap_stream_tx_caller(0x1000,0x80,0,0x1028,AP_STREAM_TX_LOCK_RETURN));
    u64 at=0,n=0;
    struct ap_stream_tx_interval v={7,100,6,0};
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,90,103,13,0,&at,&n)==1 && at==10 && n==3 && v.covered==3);
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,103,108,5,0,&at,&n)==1 && at==0 && n==3 && v.covered==6);
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,108,109,1,0,&at,&n)==2 && v.covered==6);
    v=(struct ap_stream_tx_interval){7,0xfffffffeU,4,0};
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,0xfffffffeU,2,4,0,&at,&n)==1 && n==4 && v.covered==4);
    v=(struct ap_stream_tx_interval){7,100,6,0};
    TX_CHECK(!ap_stream_tx_interval_piece(&v,8,100,106,6,0,&at,&n)); /* wrong file */
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,101,106,5,0,&at,&n)); /* gap */
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,100,106,5,0,&at,&n)); /* corrupt span */
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,100,106,6,2,&at,&n)); /* SYN */
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,100,106,6,1,&at,&n)); /* FIN */
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,100,103,3,0,&at,&n)==1);
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,102,106,4,0,&at,&n)); /* overlap */
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,100,103,3,0,&at,&n)); /* duplicate */
    TX_CHECK(v.covered!=v.length); /* missing suffix is not a complete receipt */
    v=(struct ap_stream_tx_interval){7,100,513,0};
    TX_CHECK(!ap_stream_tx_interval_piece(&v,7,100,613,513,0,&at,&n));
    v=(struct ap_stream_tx_interval){7,100,512,0};
    TX_CHECK(ap_stream_tx_interval_piece(&v,7,100,612,512,0,&at,&n)==1 && v.covered==512);
}
static void storage_controls(void) {
    TX_CHECK(ap_stream_tx_storage(4096,4100,8,16,8,4,1,1,0,0,2)); /* immutable transmit clone */
    TX_CHECK(!ap_stream_tx_storage(4096,4100,8,16,8,4,2,1,0,0,2));
    TX_CHECK(!ap_stream_tx_storage(4096,4100,8,16,8,4,1,1,1,0,2));
    TX_CHECK(!ap_stream_tx_storage(4096,4100,8,16,8,4,1,1,0,1,2));
    TX_CHECK(!ap_stream_tx_storage(4096,4100,8,16,8,4,1,18,0,0,2));
    TX_CHECK(!ap_stream_tx_storage(4096,4100,8,16,8,4,1,1,0,0,0));
    TX_CHECK(!ap_stream_tx_storage(4096,4100,9,16,8,4,1,1,0,0,2));
    TX_CHECK(!ap_stream_tx_storage(~0ULL-8,~0ULL-4,8,16,8,4,1,1,0,0,2));
    u64 sum=0;
    TX_CHECK(ap_stream_tx_fragment(0xffd4000000000000ULL,4094,2,6,&sum) && sum==2);
    TX_CHECK(ap_stream_tx_fragment(0xffd4000000000040ULL,0,4,6,&sum) && sum==6);
    TX_CHECK(!ap_stream_tx_fragment(0xffd4000000000040ULL,0,1,6,&sum));
    sum=0;TX_CHECK(!ap_stream_tx_fragment(0xffd4000000000001ULL,0,4,6,&sum));
    TX_CHECK(!ap_stream_tx_fragment(0xffd4000000000000ULL,0xffffffffU,2,6,&sum));
    TX_CHECK(!ap_stream_tx_fragment(0xffd4000000000000ULL,0,7,6,&sum));
    TX_CHECK(!ap_stream_tx_fragment(0xffd4000000000000ULL,0,0,6,&sum));
    const u64 source=ap_stream_fragment_source(0xffd4000000000000ULL,4094,4,
        0xffd4000000000000ULL,0xff11000000000000ULL);
    TX_CHECK(source==0xff11000000000ffeULL);
}
static void single_skb_controls(void) {
    struct ap_stream_tx_queue_view q={.root=0x2000,.parent=1,
        .head=0x1000,.next=0x1000,.previous=0x1000};
    TX_CHECK(ap_stream_tx_single_queue(&q)); /* one retained retransmit SKB */
    q.left=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.left=0;
    q.right=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.right=0;
    q.parent=0x3001;TX_CHECK(!ap_stream_tx_single_queue(&q));q.parent=1;
    q.queued=1;TX_CHECK(!ap_stream_tx_single_queue(&q));q.queued=0;
    q.next=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.next=q.head;
    q.previous=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));
    q=(struct ap_stream_tx_queue_view){.head=0x1000,.next=0x2000,.previous=0x2000,
        .element_next=0x1000,.element_previous=0x1000,.queued=1};
    TX_CHECK(ap_stream_tx_single_queue(&q)); /* one retained unsent SKB */
    q.queued=2;TX_CHECK(!ap_stream_tx_single_queue(&q));q.queued=1;
    q.element_next=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.element_next=q.head;
    q.element_previous=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.element_previous=q.head;
    q.previous=0x3000;TX_CHECK(!ap_stream_tx_single_queue(&q));q.previous=q.next;
    q.root=0x4000;TX_CHECK(!ap_stream_tx_single_queue(&q));
    q=(struct ap_stream_tx_queue_view){.head=0x1000,.next=0x1000,.previous=0x1000};
    TX_CHECK(!ap_stream_tx_single_queue(&q)); /* missing bytes cannot be success */
    TX_CHECK(ap_stream_tx_single_storage(4096,4100,8,16,4,0,1,0,0,0,2));
    TX_CHECK(ap_stream_tx_single_storage(4096,4100,4,16,8,8,1,1,0,0,2));
    TX_CHECK(ap_stream_tx_single_storage(4096,4100,8,16,8,4,1,1,0,0,2));
    TX_CHECK(!ap_stream_tx_single_storage(4096,4100,8,16,8,4,1,2,0,0,2));
    TX_CHECK(!ap_stream_tx_single_storage(4096,4100,8,16,8,4,1,1,1,0,2));
    TX_CHECK(!ap_stream_tx_single_storage(4096,4100,8,16,8,4,1,1,0,1,2));
    TX_CHECK(!ap_stream_tx_single_storage(4096,4100,8,16,8,4,1,1,0,0,0));
}
static struct ap_task_command command_fixture(void) {
    return (struct ap_task_command){.provider=3,.command=1,.operation=AP_ORIGINAL_SENDTO_CALL,
        .expected_object=4,.generation_before=4096,.generation_after=5,
        .expected_level=7,.expected_option=0x4000,.original_count=8};
}
static struct ap_stream_copy_record record_fixture(u32 kind,u64 sequence,u64 offset,u32 n) {
    return (struct ap_stream_copy_record){.provider=3,.command=1,.call=4,.task=10,.task_start=11,
        .sequence=sequence,.attempt=1,.offset=offset,.length=n,.kind=kind};
}
static void collector_controls(void) {
    struct ap_task_command c=command_fixture();
    TX_CHECK(ap_stream_tx_operands(&c,44,7,4096,8,0x4000,0,0));
    TX_CHECK(!ap_stream_tx_operands(&c,44,7,4096,8,0x4000,1,0));
    TX_CHECK(!ap_stream_tx_operands(&c,44,7,4096,8,0x4000,0,16));
    TX_CHECK(!ap_stream_tx_operands(&c,44,7,4096,9,0x4000,0,0));
    TX_CHECK(!ap_stream_tx_operands(&c,44,7,4096,8,0x4001,0,0));
    struct ap_session session={.incarnation=3};
    struct ap_pending_command *p=&session.pending[ap_command_slot(1)];
    p->state=AP_SLOT_ACTIVE;p->submitted=c;
    struct ap_stream_copy_record data=record_fixture(AP_STREAM_TX_DATA,1,0,3);
    memcpy(data.bytes,"ABC",3);
    TX_CHECK(!stream_tx_record(p,&data));
    TX_CHECK(stream_tx_record(p,&data)<0); /* duplicate */
    struct ap_stream_copy_record bad=record_fixture(AP_STREAM_TX_DATA,2,4,1);
    TX_CHECK(stream_tx_record(p,&bad)<0); /* hole */
    bad.offset=2;TX_CHECK(stream_tx_record(p,&bad)<0); /* overlap */
    bad.offset=3;bad.task_start++;TX_CHECK(stream_tx_record(p,&bad)<0);
    bad=record_fixture(AP_STREAM_TX_DATA,2,3,6);TX_CHECK(stream_tx_record(p,&bad)<0);
    struct ap_stream_tx_summary summary={.version=1,.file=17,.requested=8,.captured=3,
        .sequence_before=0xfffffffeU,.sequence_after=1,.protocol_returned=3,.protocol_complete=1};
    struct ap_stream_copy_record commit=record_fixture(AP_STREAM_TX_COMMIT,2,3,sizeof(summary));
    memcpy(commit.bytes,&summary,sizeof(summary));
    bad=commit;bad.bytes[sizeof(summary)]=1;TX_CHECK(stream_tx_record(p,&bad)<0);
    bad=commit;bad.offset=8;TX_CHECK(stream_tx_record(p,&bad)<0);
    bad=commit;((struct ap_stream_tx_summary *)bad.bytes)->sequence_after=2;
    TX_CHECK(stream_tx_record(p,&bad)<0);
    TX_CHECK(!stream_tx_record(p,&commit)); /* actual partial accepted prefix */
    TX_CHECK(p->stream_tx.capture.returned==3 && !memcmp(p->stream_tx.capture.bytes,"ABC",3));
    TX_CHECK(stream_tx_record(p,&commit)<0);
    p->receipt=(struct ap_command_result){.command=1,.operation=AP_ORIGINAL_SENDTO_CALL,
        .identity={.provider=3},.task=10,.start_boottime=11,.returned=3,
        .phase=AP_COMMAND_DONE,.original_count=8};
    p->original_receipt.original.selection=(struct ap_original_selection){.provider=3,
        .command=1,.call=4,.owner_mm=5,.task=10,.task_start=11,.table=13,.file=17,
        .user_address=4096,.ready=1,.requested_fd=7,.address_length=0x4000,.original_count=8};
    p->original_receipt.original.stream_tx.summary=summary;
    p->original_receipt.original.returned=3;p->original_receipt.original.complete=1;
    TX_CHECK(ap_original_result_matches(&c,&p->receipt,&p->original_receipt.original));
    p->receipt.identity.object=1;
    TX_CHECK(!ap_original_result_matches(&c,&p->receipt,&p->original_receipt.original));
    p->receipt.identity.object=0;
    ((unsigned char *)&p->receipt.state)[0]=1;
    TX_CHECK(!ap_original_result_matches(&c,&p->receipt,&p->original_receipt.original));
    ((unsigned char *)&p->receipt.state)[0]=0;
    p->original_receipt.original.address[64]=1;
    TX_CHECK(!ap_original_result_matches(&c,&p->receipt,&p->original_receipt.original));
    p->original_receipt.original.address[64]=0;
    struct ap_stream_tx_capture out;
    TX_CHECK(ap_original_sendto_capture(&session,1,&out)<0 && !p->stream_tx.read);
    p->state=AP_SLOT_COLLECTED;p->original_collected=true;
    TX_CHECK(!ap_original_sendto_capture(&session,1,&out) && p->stream_tx.read &&
        out.returned==3 && !memcmp(out.bytes,"ABC",3));
    p->stream_tx.read=false;p->original_receipt.original.selection.file++;
    TX_CHECK(ap_original_sendto_capture(&session,1,&out)<0 && !p->stream_tx.read);
    p->original_receipt.original.selection.file--;
    p->stream_tx.capture.bytes[3]=1;
    TX_CHECK(ap_original_sendto_capture(&session,1,&out)<0);
    memset(p,0,sizeof(*p));p->state=AP_SLOT_ACTIVE;p->submitted=c;
    summary=(struct ap_stream_tx_summary){.version=1,.file=17,.requested=8,
        .sequence_before=100,.sequence_after=100,.protocol_returned=(u64)(s64)-14,.protocol_complete=1};
    commit=record_fixture(AP_STREAM_TX_COMMIT,1,(u64)(s64)-14,sizeof(summary));
    memcpy(commit.bytes,&summary,sizeof(summary));
    TX_CHECK(!stream_tx_record(p,&commit) && p->stream_tx.capture.returned==-14 &&
        !p->stream_tx.capture.summary.captured && !p->stream_tx.received);
    TX_CHECK(!ap_stream_tx_summary_valid(&summary,18,8,-14));
    TX_CHECK(!ap_stream_tx_summary_valid(&summary,17,8,-11));
    memset(p,0,sizeof(*p));p->state=AP_SLOT_ACTIVE;p->submitted=c;
    bad=record_fixture(AP_STREAM_TX_COMMIT,1,8,sizeof(summary));
    summary=(struct ap_stream_tx_summary){.version=1,.file=17,.requested=8,.captured=8,
        .sequence_before=100,.sequence_after=108,.protocol_returned=8,.protocol_complete=1};
    memcpy(bad.bytes,&summary,sizeof(summary));
    TX_CHECK(stream_tx_record(p,&bad)<0 && !p->stream_tx.committed); /* no bytes is not capture */
}
static void blocking_collector_controls(void) {
    struct ap_session session={.incarnation=3};struct ap_pending_command *p=&session.pending[1];
    p->state=AP_SLOT_ACTIVE;p->submitted=command_fixture();
    p->submitted.operation=AP_ORIGINAL_SENDTO_BLOCKING_CALL;p->submitted.expected_timeout_ticks=5000;
    struct ap_stream_copy_record data=record_fixture(AP_STREAM_TX_DATA,1,0,3);memcpy(data.bytes,"ABC",3);
    TX_CHECK(stream_tx_record(p,&data)<0); /* operation selects layout before payload */
    TX_CHECK(!stream_tx_blocking_record(p,&data));
    struct ap_stream_tx_blocking_summary summary={.prefix={.version=2,.file=17,.requested=8,.captured=3,
        .sequence_before=0xfffffffeU,.sequence_after=1,.protocol_returned=3,.protocol_complete=1},
        .saved_timeout_ticks=5000};
    struct ap_stream_copy_record commit=record_fixture(AP_STREAM_TX_COMMIT,2,3,sizeof(summary));
    memcpy(commit.bytes,&summary,sizeof(summary));
    struct ap_stream_copy_record bad=commit;bad.length=64;
    TX_CHECK(stream_tx_blocking_record(p,&bad)<0);
    bad=commit;bad.bytes[0]=1;TX_CHECK(stream_tx_blocking_record(p,&bad)<0);
    bad=commit;bad.bytes[64]^=1;TX_CHECK(stream_tx_blocking_record(p,&bad)<0);
    bad=commit;bad.bytes[72]=1;TX_CHECK(stream_tx_blocking_record(p,&bad)<0);
    p->submitted.expected_timeout_ticks++;TX_CHECK(stream_tx_blocking_record(p,&commit)<0);p->submitted.expected_timeout_ticks--;
    TX_CHECK(!p->stream_tx_blocking.committed);
    TX_CHECK(!stream_tx_blocking_record(p,&commit));
    TX_CHECK(p->stream_tx_blocking.committed && p->stream_tx_blocking.capture.summary.saved_timeout_ticks==5000);
    TX_CHECK(stream_tx_blocking_record(p,&commit)<0);
    /* A v2 result cannot leak into old v1 even when its first eight values look
     * plausible. The separately named API retains the original624-byte type. */
    struct {u64 pre;struct ap_stream_tx_capture value;u64 post;} old={.pre=0xfeed,.post=0xbeef};
    TX_CHECK(ap_original_sendto_capture(&session,1,&old.value)<0);
    TX_CHECK(old.pre==0xfeed && old.post==0xbeef);
}
int main(void) {
    image_controls();blocking_image_controls();interval_controls();storage_controls();single_skb_controls();collector_controls();blocking_collector_controls();
    printf("stream TX shared decoder/collector checks: %u\n",checks);return 0;
}
