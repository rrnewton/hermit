#define _GNU_SOURCE
#include <assert.h>
#include <stdlib.h>
#include "retirement-target.h"
#include "grouped-target.h"

struct copy_image_fixture { off64_t position;long corrupt,truncate; };
static off64_t copy_image_size(void) {
    off64_t end=0;
    for(size_t i=0;i<sizeof(ap_group_image_slices)/sizeof(ap_group_image_slices[0]);i++) {
        const struct ap_copy_image_slice *slice=&ap_group_image_slices[i];
        off64_t next=slice->offset+(off64_t)slice->size;if(next>end)end=next;
    }
    return end;
}
/* Buffered stdio may seek to an aligned offset in the surrounding ELF gap,
 * then use SEEK_CUR after read-ahead. Model those regular-file operations;
 * an unrepresented gap is zero-filled data, not premature EOF. */
static ssize_t copy_image_read(void *opaque,char *out,size_t size) {
    struct copy_image_fixture *fixture=opaque;
    off64_t end=fixture->truncate>=0?fixture->truncate:copy_image_size();
    if(fixture->position>=end)return 0;
    if(size>(size_t)(end-fixture->position))size=end-fixture->position;
    memset(out,0,size);
    for(size_t i=0;i<sizeof(ap_group_image_slices)/sizeof(ap_group_image_slices[0]);i++) {
        const struct ap_copy_image_slice *slice=&ap_group_image_slices[i];
        off64_t first=fixture->position>slice->offset?fixture->position:slice->offset;
        off64_t last=fixture->position+(off64_t)size,limit=slice->offset+(off64_t)slice->size;
        if(last>limit)last=limit;
        if(first<last)memcpy(out+(first-fixture->position),slice->bytes+(first-slice->offset),last-first);
    }
    if(fixture->corrupt>=fixture->position && fixture->corrupt<fixture->position+(off64_t)size)
        out[fixture->corrupt-fixture->position]^=1;
    fixture->position+=size;return size;
}
static int copy_image_seek(void *opaque,off64_t *offset,int whence) {
    struct copy_image_fixture *fixture=opaque;
    off64_t base;
    if(whence==SEEK_SET)base=0;
    else if(whence==SEEK_CUR)base=fixture->position;
    else if(whence==SEEK_END)base=fixture->truncate>=0?fixture->truncate:copy_image_size();
    else {errno=EINVAL;return -1;}
    if((*offset<0 && *offset < -base) || (*offset>=0 && *offset>INT64_MAX-base)) {errno=EINVAL;return -1;}
    fixture->position=base+*offset;*offset=fixture->position;return 0;
}
static FILE *copy_image_fixture_open(struct copy_image_fixture *fixture) {
    FILE *input=fopencookie(fixture,"r",(cookie_io_functions_t){.read=copy_image_read,.seek=copy_image_seek});
    if(input)assert(!setvbuf(input,NULL,_IOFBF,4096));return input;
}
int main(void) {
    (void)ap_require_grouped_target;
    unsigned controls=0,bytes=0;
    for(size_t i=0;i<sizeof(ap_group_image_slices)/sizeof(ap_group_image_slices[0]);i++) {
        const struct ap_copy_image_slice *slice=&ap_group_image_slices[i];
        for(size_t at=0;at<slice->size;at++) {
            struct copy_image_fixture fixture={.corrupt=slice->offset+(long)at,.truncate=-1};
            FILE *input=copy_image_fixture_open(&fixture);assert(input);
            errno=0;int rc=ap_check_grouped_image(input),error=errno; if(rc!=-1 || error!=ESTALE)fprintf(stderr,"mutation slice=%zu at=%zu offset=%lld rc=%d errno=%d position=%lld\n",i,at,(long long)slice->offset,rc,error,(long long)fixture.position);assert(rc==-1 && error==ESTALE);assert(!fclose(input));controls++;bytes++;
        }
        struct copy_image_fixture fixture={.corrupt=-1,.truncate=slice->offset+(long)slice->size-1};
        FILE *input=copy_image_fixture_open(&fixture);assert(input);
        errno=0;assert(ap_check_grouped_image(input)==-1 && errno==ENODATA);assert(!fclose(input));controls++;
    }
    struct copy_image_fixture fixture={.corrupt=-1,.truncate=-1};
    FILE *input=copy_image_fixture_open(&fixture);assert(input);assert(!ap_check_grouped_image(input));assert(!fclose(input));controls++;
    /* Original 26,334 bytes remain covered. Add both complete272-byte
     * dispatcher tables and the full3,621-byte Btrfs ioctl switch. Every byte
     * is independently corrupted, and each of all33 slices truncated. */
    assert(bytes==30499 && controls==30533);
    printf("GROUPED_IMAGE bytes=%u slices=33 mutations_truncations_positive=%u\n",bytes,controls);return 0;
}
