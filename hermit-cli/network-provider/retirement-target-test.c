#define _GNU_SOURCE
#include <assert.h>
#include <stdlib.h>
#include <stdarg.h>
#include <stdio.h>
static unsigned sub_symbol_scan_calls;
static int sub_symbol_scan(const char *line,const char *format,...) {
    sub_symbol_scan_calls++;
    va_list args;va_start(args,format);
    int rc=vsscanf(line,format,args);va_end(args);return rc;
}
static FILE *sub_target_fopen(const char *,const char *);
#define fopen sub_target_fopen
#define sscanf sub_symbol_scan
#include "retirement-target.h"
#undef sscanf
#undef fopen

/* The reference is the unchanged libc conversion, not a second manual
 * parser. Every fast success must have exactly its three field values. */
static void sub_fast_fields_equal(const char *line) {
    char old_address[17]={0},old_type=0,old_symbol[512]={0},old_extra=0;
    char address[17]={0},type=0,symbol[512]={0};
    int fields=sscanf(line,"%16[0123456789abcdefABCDEF] %c %511s %c",
        old_address,&old_type,old_symbol,&old_extra);
    if(sub_retirement_fields_fast(line,strlen(line),address,&type,symbol)) {
        assert(fields==3);
        assert(!strcmp(address,old_address) && type==old_type && !strcmp(symbol,old_symbol));
    }
}
static void sub_retirement_fast_controls(void) {
    const char good[]="0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n";
    char address[17],type,symbol[512],changed[sizeof(good)];
    assert(sub_retirement_fields_fast(good,strlen(good),address,&type,symbol));
    assert(!strcmp(symbol,AP_FILE_RETIRE_SYMBOL));
    for(size_t at=0;at<sizeof(good)-1;at++)for(unsigned byte=0;byte<256;byte++) {
        memcpy(changed,good,sizeof(good));changed[at]=(char)byte;
        sub_fast_fields_equal(changed);
    }
    const char *fallback[]={"0000000000000000 t " AP_FILE_RETIRE_SYMBOL " [module]\n",
        "g000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n",
        "0000000000000000\tt\t" AP_FILE_RETIRE_SYMBOL "\n",
        "0000000000000000 t " AP_FILE_RETIRE_SYMBOL " \n"};
    for(size_t i=0;i<sizeof(fallback)/sizeof(fallback[0]);i++) {
        assert(!sub_retirement_fields_fast(fallback[i],strlen(fallback[i]),address,&type,symbol));
        sub_fast_fields_equal(fallback[i]);
    }
    char wide[533];memcpy(wide,"0000000000000000 t ",19);
    memset(wide+19,'s',511);wide[530]='\n';wide[531]=0;
    assert(sub_retirement_fields_fast(wide,strlen(wide),address,&type,symbol));
    sub_fast_fields_equal(wide);
    wide[530]='s';wide[531]='\n';wide[532]=0;
    assert(!sub_retirement_fields_fast(wide,strlen(wide),address,&type,symbol));
    sub_fast_fields_equal(wide);
    /* Exercise the actual full stream checker, not just its helper. The
     * fallback interposer delegates unchanged conversions to libc. */
    const char complete[]="0000000000000000 t ptrace_request\n"
        "0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n"
        "0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n";
    FILE *input=fmemopen((void *)complete,strlen(complete),"r");assert(input);
    sub_symbol_scan_calls=0;
    assert(!ap_check_retirement_symbols(input));assert(!fclose(input));
    assert(sub_symbol_scan_calls==0);
    const char fallback_complete[]="0000000000000000\tt\tptrace_request\n"
        "0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n"
        "0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n";
    input=fmemopen((void *)fallback_complete,strlen(fallback_complete),"r");assert(input);
    sub_symbol_scan_calls=0;
    assert(!ap_check_retirement_symbols(input));assert(!fclose(input));
    assert(sub_symbol_scan_calls==1);
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
/* Only fopen is redirected in the production-header inclusion above.
 * These are real stdio streams: libc performs reads, EOF/error state and
 * fclose; the cookie callbacks supply controlled bytes and actual errors. */
struct sub_stream {
    const char *data;size_t size,position;
    int read_error,close_error;
    unsigned eof_calls,read_failures,close_calls,close_failures;
    bool is_image;struct copy_image_fixture image;
};
static ssize_t sub_stream_read(void *opaque,char *out,size_t size) {
    struct sub_stream *stream=opaque;
    if(stream->is_image)return copy_image_read(&stream->image,out,size);
    if(!size)return 0;
    if(stream->position==stream->size) {
        if(stream->read_error) {
            stream->read_failures++;errno=stream->read_error;return -1;
        }
        stream->eof_calls++;return 0;
    }
    size_t left=stream->size-stream->position;if(size>left)size=left;
    memcpy(out,stream->data+stream->position,size);
    stream->position+=size;return size;
}
static int sub_stream_seek(void *opaque,off64_t *offset,int whence) {
    struct sub_stream *stream=opaque;assert(stream->is_image);
    return copy_image_seek(&stream->image,offset,whence);
}
static int sub_stream_close(void *opaque) {
    struct sub_stream *stream=opaque;stream->close_calls++;
    if(stream->close_error) {
        stream->close_failures++;errno=stream->close_error;return -1;
    }
    return 0;
}
static FILE *sub_stream_open(struct sub_stream *stream) {
    FILE *input=fopencookie(stream,"r",(cookie_io_functions_t){
        .read=sub_stream_read,.seek=stream->is_image?sub_stream_seek:NULL,
        .close=sub_stream_close});
    /* Preserve the existing sparse image fixture's exact seek/read mode. */
    if(input && stream->is_image)assert(!setvbuf(input,NULL,_IONBF,0));
    return input;
}
static const char sub_required_symbols[]=
    "0000000000000000 t ptrace_request\n"
    "0000000000000000 t filp_close\n"
    "0000000000000000 t do_close_on_exec\n"
    "0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n";
static unsigned sub_scanner_fault_cases,sub_caller_fault_cases;
static void sub_scanner_result(struct sub_stream *stream,int expected,bool read_error) {
    FILE *input=sub_stream_open(stream);assert(input);
    errno=0;int rc=ap_check_retirement_symbols(input),error=errno;
    bool actual_read_error=ferror(input)!=0;
    int closed=fclose(input);
    /* Semantic assertions occur only after the original FILE is closed. */
    assert(closed==0 && stream->close_calls==1 && !stream->close_failures);
    assert(actual_read_error==read_error);
    assert(expected ? rc==-1 && error==expected : rc==0);
    assert(!read_error || (stream->read_failures && stream->position==stream->size));
    sub_scanner_fault_cases++;
}
struct sub_caller { struct sub_stream files[3];unsigned opened; };
static struct sub_caller *sub_active_caller;
static FILE *sub_target_fopen(const char *path,const char *mode) {
    assert(sub_active_caller && !strcmp(mode,"re"));
    unsigned kind;
    if(!strcmp(path,"/sys/kernel/notes"))kind=0;
    else if(!strcmp(path,"/proc/kallsyms"))kind=1;
    else {assert(!strcmp(path,AP_COPY_KERNEL_IMAGE));kind=2;}
    assert(sub_active_caller->opened==kind);
    sub_active_caller->opened++;
    return sub_stream_open(&sub_active_caller->files[kind]);
}
static void sub_retirement_fault_controls(void) {
    struct sub_stream stream={.data=sub_required_symbols,
        .size=sizeof(sub_required_symbols)-1,.read_error=EIO};
    sub_scanner_result(&stream,EIO,true);
    const struct { const char *row;int expected; } late[]={
        {"not a kallsyms line\n",EPROTO},
        {"0000000000000000 t ptrace_request\n",ESTALE},
        {"0000000000000000 t " AP_FILE_RETIRE_SYMBOL " [module]\n",ESTALE},
        {"0000000000000000 t __fput.llvm.1\n",ESTALE},
        {"0000000000000000 t sub_unrelated",EOVERFLOW},
    };
    char joined[1024];
    for(size_t i=0;i<sizeof(late)/sizeof(late[0]);i++) {
        assert(sizeof(sub_required_symbols)+strlen(late[i].row)<=sizeof(joined));
        strcpy(joined,sub_required_symbols);strcat(joined,late[i].row);
        stream=(struct sub_stream){.data=joined,.size=strlen(joined)};
        sub_scanner_result(&stream,late[i].expected,false);
    }
    /* Both sizes contain only complete, grammatically valid rows; the extra
     * byte extends the last symbol, so only the aggregate cap distinguishes. */
    const size_t cap=16U*1024U*1024U;
    const char filler[]="0000000000000000 t sub_unrelated\n";
    char *large=malloc(cap+1);assert(large);
    size_t at=sizeof(sub_required_symbols)-1;
    memcpy(large,sub_required_symbols,at);
    while(cap-at>=2*(sizeof(filler)-1)) {
        memcpy(large+at,filler,sizeof(filler)-1);at+=sizeof(filler)-1;
    }
    size_t tail=cap-at;assert(tail>=21 && tail<=531);
    memcpy(large+at,"0000000000000000 t ",19);
    memset(large+at+19,'s',tail-20);large[cap-1]='\n';
    stream=(struct sub_stream){.data=large,.size=cap};
    sub_scanner_result(&stream,0,false);
    assert(stream.position==cap && stream.eof_calls);
    large[cap-1]='s';large[cap]='\n';
    stream=(struct sub_stream){.data=large,.size=cap+1};
    sub_scanner_result(&stream,EOVERFLOW,false);
    assert(stream.position==cap+1);free(large);

    const struct {
        int close_at,expected;unsigned opened;
        bool notes_read,symbols_read,bad_symbols,bad_image,bad_notes;
    } cases[]={
        {.close_at=-1,.opened=3},
        {.close_at=0,.expected=ENOSPC,.opened=1},
        {.close_at=1,.expected=ENOSPC,.opened=2},
        {.close_at=2,.expected=ENOSPC,.opened=3},
        {.close_at=0,.expected=EIO,.opened=1,.notes_read=true},
        {.close_at=1,.expected=EIO,.opened=2,.symbols_read=true},
        {.close_at=1,.expected=EPROTO,.opened=2,.bad_symbols=true},
        {.close_at=2,.expected=ESTALE,.opened=3,.bad_image=true},
        {.close_at=-1,.expected=ESTALE,.opened=1,.bad_notes=true},
        /* Notes are validated after fclose: its failure precedes a still
         * unobserved bad build ID, unlike an already-observed read failure. */
        {.close_at=0,.expected=ENOSPC,.opened=1,.bad_notes=true},
    };
    unsigned char changed_notes[sizeof(ap_copy_image_notes)];
    /* This full notes fixture has two Linux notes before GNU's build ID. */
    assert(sizeof(changed_notes)==84);
    memcpy(changed_notes,ap_copy_image_notes,sizeof(changed_notes));changed_notes[64]^=1;
    strcpy(joined,sub_required_symbols);strcat(joined,"not a kallsyms line\n");
    for(size_t i=0;i<sizeof(cases)/sizeof(cases[0]);i++) {
        struct sub_caller caller={.files={
            {.data=(const char *)ap_copy_image_notes,.size=sizeof(ap_copy_image_notes)},
            {.data=sub_required_symbols,.size=sizeof(sub_required_symbols)-1},
            {.is_image=true,.image={.corrupt=-1,.truncate=-1}},
        }};
        if(cases[i].close_at>=0)caller.files[cases[i].close_at].close_error=ENOSPC;
        if(cases[i].notes_read)caller.files[0].read_error=EIO;
        if(cases[i].symbols_read)caller.files[1].read_error=EIO;
        if(cases[i].bad_symbols) {caller.files[1].data=joined;caller.files[1].size=strlen(joined);}
        if(cases[i].bad_image)caller.files[2].image.corrupt=ap_copy_image_slices[0].offset;
        if(cases[i].bad_notes)caller.files[0].data=(const char *)changed_notes;
        assert(!sub_active_caller);sub_active_caller=&caller;
        errno=0;int rc=ap_require_retirement_target(),error=errno;
        sub_active_caller=NULL;
        assert(caller.opened==cases[i].opened);
        for(unsigned j=0;j<3;j++) {
            assert(caller.files[j].close_calls==(j<caller.opened));
            assert(caller.files[j].close_failures==
                (j<caller.opened && cases[i].close_at==(int)j));
        }
        assert(cases[i].expected ? rc==-1 && error==cases[i].expected : rc==0);
        assert(!cases[i].notes_read || (caller.files[0].read_failures &&
            caller.files[0].position==caller.files[0].size));
        assert(!cases[i].symbols_read || (caller.files[1].read_failures &&
            caller.files[1].position==caller.files[1].size));
        sub_caller_fault_cases++;
    }
    assert(sub_scanner_fault_cases==8 && sub_caller_fault_cases==10);
    printf("sub_retirement_fault_controls scanner=%u caller=%u all_closed=1\n",
        sub_scanner_fault_cases,sub_caller_fault_cases);
}

static void retirement_link_controls(void) {
    unsigned controls=0;
#define LINK_CHECK(value) do {assert(value);controls++;} while(0)
    const uint64_t address=0xffffffff820720f0ULL;
    struct bpf_prog_info program={.type=BPF_PROG_TYPE_KPROBE,.id=37};
    struct bpf_link_info link={.type=BPF_LINK_TYPE_KPROBE_MULTI,.id=41,.prog_id=37};
    link.kprobe_multi.count=1;
    LINK_CHECK(ap_retirement_link_matches(0,address,&program,sizeof(program),
        &link,sizeof(link),address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(ap_retirement_link_matches(1,AP_EXEC_CLOSE_IMAGE,&program,sizeof(program),
        &link,sizeof(link),AP_EXEC_CLOSE_IMAGE,AP_EXEC_CLOSE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(2,address,&program,sizeof(program),
        &link,sizeof(link),address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,0,&program,sizeof(program),
        &link,sizeof(link),address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,address,&program,sizeof(program),
        &link,sizeof(link),address+1,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,address,&program,sizeof(program),
        &link,sizeof(link),address,AP_FILE_RETIRE_COOKIE+1));
    struct bpf_prog_info good_program=program;struct bpf_link_info good_link=link;
#define BAD_PROGRAM(field,value) do {program=good_program;program.field=value; \
    LINK_CHECK(!ap_retirement_link_matches(0,address,&program,sizeof(program), \
        &good_link,sizeof(good_link),address,AP_FILE_RETIRE_COOKIE));} while(0)
#define BAD_LINK(field,value) do {link=good_link;link.field=value; \
    LINK_CHECK(!ap_retirement_link_matches(0,address,&good_program,sizeof(good_program), \
        &link,sizeof(link),address,AP_FILE_RETIRE_COOKIE));} while(0)
    BAD_PROGRAM(type,BPF_PROG_TYPE_TRACING);BAD_PROGRAM(id,0);BAD_PROGRAM(recursion_misses,1);
    BAD_LINK(type,BPF_LINK_TYPE_PERF_EVENT);BAD_LINK(id,0);BAD_LINK(prog_id,38);
    BAD_LINK(kprobe_multi.count,2);BAD_LINK(kprobe_multi.flags,BPF_F_KPROBE_MULTI_RETURN);
    BAD_LINK(kprobe_multi.missed,1);
    LINK_CHECK(!ap_retirement_link_matches(0,address,&good_program,0,
        &good_link,sizeof(good_link),address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,address,&good_program,sizeof(good_program),
        &good_link,0,address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,address,0,sizeof(good_program),
        &good_link,sizeof(good_link),address,AP_FILE_RETIRE_COOKIE));
    LINK_CHECK(!ap_retirement_link_matches(0,address,&good_program,sizeof(good_program),
        0,sizeof(good_link),address,AP_FILE_RETIRE_COOKIE));
    assert(controls==19);printf("ftrace retirement KPROBE_MULTI admission controls=%u\n",controls);
#undef BAD_LINK
#undef BAD_PROGRAM
#undef LINK_CHECK
}
static unsigned controls;
static void note_result(const unsigned char *notes,size_t size,int expected) {
    errno=0;int rc=ap_check_kernel_notes(notes,size);
    assert(expected ? rc==-1 && errno==expected : rc==0);controls++;
}
static void symbol_exact_result(const char *text,int expected) {
    FILE *f=fmemopen((void *)text,strlen(text),"r");assert(f);
    errno=0;int rc=ap_check_retirement_symbols(f);int error=errno;assert(fclose(f)==0);
    assert(expected ? rc==-1 && error==expected : rc==0);controls++;
}
/* The same original nineteen controls now receive the two additionally
 * required exact core symbols. Their original expectations remain unchanged. */
static void symbol_result(const char *text,int expected) {
    const char *prefix="0000000000000000 t ptrace_request\n0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n";
    char joined[2048];assert(strlen(prefix)+strlen(text)<sizeof(joined));
    strcpy(joined,prefix);strcat(joined,text);symbol_exact_result(joined,expected);
}
int main(void) {
    (void)ap_require_retirement_target;
    sub_retirement_fast_controls();
    sub_retirement_fault_controls();
    retirement_link_controls();
    const unsigned char good[]={4,0,0,0,20,0,0,0,3,0,0,0,'G','N','U',0,
        0xc9,0x40,0x78,0x92,0xac,0xd3,0x01,0x14,0x61,0x91,0xb0,0x0f,0x42,0x3f,0x26,0x12,0xbf,0x8b,0x2d,0x6a};
    note_result(good,sizeof(good),0);note_result(good,0,ENODATA);
    note_result(good,11,EPROTO);note_result(good,sizeof(good)-1,EPROTO);
    unsigned char changed[sizeof(good)];memcpy(changed,good,sizeof(good));changed[16]^=1;
    note_result(changed,sizeof(changed),ESTALE);
    memcpy(changed,good,sizeof(good));changed[8]=2;note_result(changed,sizeof(changed),ENODATA);
    memcpy(changed,good,sizeof(good));changed[4]=19;note_result(changed,sizeof(changed),ESTALE);
    unsigned char twice[2*sizeof(good)];memcpy(twice,good,sizeof(good));memcpy(twice+sizeof(good),good,sizeof(good));
    note_result(twice,sizeof(twice),ESTALE);
    memcpy(changed,good,sizeof(good));memset(changed,255,4);note_result(changed,sizeof(changed),EPROTO);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n",0);
    symbol_result("ffffffff12345678 t " AP_FILE_RETIRE_SYMBOL "\nffffffff22345678 T unrelated [module]\n",0);
    symbol_result("0000000000000000 t unrelated\n",ENODATA);
    symbol_result("0000000000000000 t __fput\n",ESTALE);
    symbol_result("0000000000000000 t __fput.llvm.1\n",ESTALE);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL " [module]\n",ESTALE);
    symbol_result("0000000000000000 T " AP_FILE_RETIRE_SYMBOL "\n",ESTALE);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n",ESTALE);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL,EOVERFLOW);
    symbol_result("not a kallsyms line\n",EPROTO);
    assert(controls==19);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n",ENODATA);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n",ENODATA);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t do_close_on_exec\n",ENODATA);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n",ESTALE);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t do_close_on_exec\n",ESTALE);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close [module]\n0000000000000000 t do_close_on_exec\n",ESTALE);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n0000000000000000 T do_close_on_exec\n",ESTALE);
    assert(controls==26);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n",ENODATA);
    symbol_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t ptrace_request\n",ESTALE);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n0000000000000000 t ptrace_request [module]\n",ESTALE);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n0000000000000000 T ptrace_request\n",ESTALE);
    symbol_exact_result("0000000000000000 t " AP_FILE_RETIRE_SYMBOL "\n0000000000000000 t filp_close\n0000000000000000 t do_close_on_exec\n0000000000000000 t ptrace_request\n",0);
    assert(controls==31);printf("retirement and exec target controls=%u passed\n",controls);
    unsigned selection_controls=0;
#define SITE_CHECK(v) do { assert(v);selection_controls++; } while(0)
    SITE_CHECK(ap_accept_fdget_site(0x1000,0x1021));
    SITE_CHECK(!ap_accept_fdget_site(0,0x21));
    SITE_CHECK(!ap_accept_fdget_site(0x1000,0x1020));
    SITE_CHECK(!ap_accept_fdget_site(0x1000,0x1022));
    SITE_CHECK(!ap_accept_fdget_site(~0ULL-0x20,0));
    SITE_CHECK(ap_accept_fdget_site(~0ULL-0x21,~0ULL));
    SITE_CHECK(!ap_accept_fdget_site(0x2000,0x1021));
    assert(selection_controls==7);
    printf("original accept fdget call site controls=%u\n",selection_controls);
    /* Literal return offsets are bound to retained __sys_connect disassembly,
     * not copied from the constants under test. Wrong direct caller, missing
     * base, adjacent site and wrapped arithmetic must all refuse. */
    struct { int (*site)(unsigned long long,unsigned long long);unsigned long long offset; } connect_sites[]={
        {ap_connect_fdget_site,0x1c},{ap_connect_copy_site,0x46},
        {ap_connect_security_site,0x97},{ap_connect_audit_site,0xdf}};
    unsigned connect_controls=0;
#define CONNECT_SITE(v) do { assert(v);connect_controls++; } while(0)
    for(unsigned i=0;i<4;i++) {
        unsigned long long offset=connect_sites[i].offset;
        CONNECT_SITE(connect_sites[i].site(0x1000,0x1000+offset));
        CONNECT_SITE(!connect_sites[i].site(0,offset));
        CONNECT_SITE(!connect_sites[i].site(0x1000,0x1000+offset-1));
        CONNECT_SITE(!connect_sites[i].site(0x1000,0x1000+offset+1));
        CONNECT_SITE(!connect_sites[i].site(0x2000,0x1000+offset));
        CONNECT_SITE(!connect_sites[i].site(~0ULL-offset+1,0));
        CONNECT_SITE(connect_sites[i].site(~0ULL-offset,~0ULL));
    }
    assert(connect_controls==28);
    printf("original connect direct call sites=%u controls\n",connect_controls);
    unsigned image_controls=0;
    for(size_t i=0;i<sizeof(ap_copy_image_slices)/sizeof(ap_copy_image_slices[0]);i++) {
        const struct ap_copy_image_slice *slice=&ap_copy_image_slices[i];
        for(size_t at=0;at<slice->size;at++) {
            struct copy_image_fixture fixture={.corrupt=slice->offset+(long)at,.truncate=-1};
            FILE *input=copy_image_fixture_open(&fixture);assert(input);
            errno=0;assert(ap_check_copy_image(input)==-1 && errno==ESTALE);assert(!fclose(input));image_controls++;
        }
        struct copy_image_fixture fixture={.corrupt=-1,.truncate=slice->offset+(long)slice->size-1};
        FILE *input=copy_image_fixture_open(&fixture);assert(input);
        errno=0;assert(ap_check_copy_image(input)==-1 && errno==ENODATA);assert(!fclose(input));image_controls++;
    }
    struct copy_image_fixture fixture={.corrupt=-1,.truncate=-1};
    FILE *input=copy_image_fixture_open(&fixture);assert(input);
    assert(!ap_check_copy_image(input));assert(!fclose(input));image_controls++;
    printf("copy installed-image exact ELF/text: %u mutation/truncation/positive controls\n",image_controls);
    return 0;
}
