#define _GNU_SOURCE
#include <assert.h>
#include <stdlib.h>
#include "retirement-target.h"

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
