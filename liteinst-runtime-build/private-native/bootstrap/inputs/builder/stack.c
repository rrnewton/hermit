#define _GNU_SOURCE
#include "private_startup.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static _Alignas(16) unsigned char guest[4096], image[8192], stack[131072];
static unsigned char before[sizeof guest], image_before[sizeof image];
static const unsigned char name[] = "/private/detcore-crt";
static struct ps_request request;
static unsigned int checks;
static size_t aux_offset;

static void put(unsigned char *where, uint64_t value, size_t length)
{ for (size_t index = 0; index < length; ++index) where[index] = value >> (index * 8); }
static uint64_t get(const void *where)
{ uint64_t value; memcpy(&value, where, 8); return value; }
static void phdr(size_t index, uint32_t type, uint32_t flags, uint64_t offset,
                 uint64_t address, uint64_t filesz, uint64_t memsz, uint64_t alignment)
{
  unsigned char *header = image + 64 + index * 56;
  put(header, type, 4); put(header + 4, flags, 4);
  put(header + 8, offset, 8); put(header + 16, address, 8);
  put(header + 32, filesz, 8); put(header + 40, memsz, 8);
  put(header + 48, alignment, 8);
}
static void fixture(void)
{
  memset(guest, 0, sizeof guest); memset(image, 0, sizeof image);
  memset(stack, 0xa5, sizeof stack);
  memcpy(image, "\177ELF\2\1\1", 7);
  put(image + 16, 3, 2); put(image + 18, 62, 2); put(image + 20, 1, 4);
  put(image + 24, 0x400, 8); put(image + 32, 64, 8);
  put(image + 52, 64, 2); put(image + 54, 56, 2); put(image + 56, 4, 2);
  phdr(0, 1, 5, 0, 0, 4096, 4096, 4096);
  phdr(1, 1, 6, 4096, 8192, 2048, 4096, 4096);
  phdr(2, 2, 6, 4352, 8448, 16, 16, 8);
  phdr(3, 7, 4, 4608, 8704, 32, 64, 8);
  strcpy((char *) guest + 2048, "/guest/app");
  strcpy((char *) guest + 2080, "arg");
  strcpy((char *) guest + 2120, "REVERIE_TEST_FD=17");
  strcpy((char *) guest + 2160, "RUST_LOG=info");
  strcpy((char *) guest + 2200, "x86_64");
  strcpy((char *) guest + 2240, "baseline");
  for (size_t index = 0; index < 16; ++index) guest[2304 + index] = index * 7;
  uint64_t words[] = {2, (uintptr_t) guest + 2048, (uintptr_t) guest + 2080, 0,
    (uintptr_t) guest + 2120, (uintptr_t) guest + 2160, 0,
    3, 0x400040, 4, 56, 5, 9, 6, 4096, 7, 0x7f000000, 9, 0x401000,
    23, 0, 25, (uintptr_t) guest + 2304, 31, (uintptr_t) guest + 2048,
    15, (uintptr_t) guest + 2200, 24, (uintptr_t) guest + 2240,
    33, 0x7fffffe000, 16, 0x123, 26, 0x456, 11, 1000, 12, 1000,
    13, 1000, 14, 1000, 27, 20, 28, 32, 51, 11952, 0, 0};
  memcpy(guest, words, sizeof words);
  aux_offset = 56;
  request = (struct ps_request) {{guest, sizeof guest}, {image, sizeof image},
    0x10000000, 16384, 0x500, {name, sizeof name}, 65536};
}
static unsigned char *aux(uint64_t type)
{
  for (size_t offset = aux_offset; get(guest + offset); offset += 16)
    if (get(guest + offset) == type) return guest + offset + 8;
  abort();
}
static size_t table_end(void)
{
  size_t offset = aux_offset;
  while (get(guest + offset)) offset += 16;
  return offset + 16;
}
static uintptr_t private_aux(struct ps_launch *launch, uint64_t type)
{
  unsigned char *cursor = (void *) (launch->crt_sp + 24);
  while (get(cursor)) cursor += 8;
  cursor += 8;
  while (get(cursor)) {
    if (get(cursor) == type) return get(cursor + 8);
    cursor += 16;
  }
  abort();
}
static int private_pointer(uintptr_t pointer, size_t length)
{ return pointer >= (uintptr_t) stack && length <= sizeof stack &&
    pointer - (uintptr_t) stack <= sizeof stack - length; }
static void success(void)
{
  memcpy(before, guest, sizeof guest); memcpy(image_before, image, sizeof image);
  size_t required = 0;
  assert(ps_measure(&request, &required) == PS_OK);
  assert(required <= sizeof stack && required % 16 == 0);
  struct ps_launch launch;
  assert(ps_build(&request, stack, required, &launch) == PS_OK);
  assert(launch.crt_entry == request.image_bias + 0x500 && launch.crt_rdx == 0);
  assert(launch.crt_entry != request.image_bias + get(image + 24));
  assert(launch.crt_sp % 16 == 0 && launch.crt_sp >= (uintptr_t) stack + 65536);
  assert(get((void *) launch.crt_sp) == 1 && get((void *) (launch.crt_sp + 16)) == 0);
  uintptr_t argv0 = get((void *) (launch.crt_sp + 8));
  assert(private_pointer(argv0, sizeof name) && strcmp((char *) argv0, (char *) name) == 0);
  assert(private_aux(&launch, 3) == request.image_bias + 64);
  assert(private_aux(&launch, 4) == 56 && private_aux(&launch, 5) == 4);
  assert(private_aux(&launch, 7) == 0 && private_aux(&launch, 9) == launch.crt_entry);
  assert(private_aux(&launch, 31) == argv0);
  assert(private_aux(&launch, 33) == 0 && get(aux(33)) == 0x7fffffe000);
  assert(private_aux(&launch, PS_AUX_HANDOFF) == (uintptr_t) launch.handoff);
  assert(launch.handoff->version == PS_ABI_VERSION && launch.handoff->size == 104);
  assert(launch.handoff->original_sp == (uintptr_t) guest);
  assert(launch.handoff->original_stack_end == (uintptr_t) guest + sizeof guest);
  assert(launch.handoff->original_argc == get(guest));
  assert(launch.handoff->original_argv == (uintptr_t) guest + 8);
  assert(launch.handoff->original_envp == (uintptr_t) guest + (get(guest) + 2) * 8);
  assert(launch.handoff->original_auxv == (uintptr_t) guest + aux_offset);
  assert(launch.handoff->original_auxc == (table_end() - aux_offset) / 16 - 1);
  assert(launch.handoff->original_random == get(aux(25)));
  assert(launch.handoff->private_stack_end == (uintptr_t) stack + required);
  assert(launch.handoff->private_stack_begin == (uintptr_t) stack);
  assert((uintptr_t) launch.handoff % 8 == 0);
  for (size_t offset = aux_offset; get(guest + offset); offset += 16) {
    uint64_t type = get(guest + offset);
    uintptr_t value = get(guest + offset + 8), actual = private_aux(&launch, type);
    if (type == 3 || type == 4 || type == 5 || type == 7 || type == 9 || type == 31 || type == 33) continue;
    if (type == 15 || type == 24 || type == 25) {
      size_t length = type == 25 ? 16 : strlen((char *) value) + 1;
      assert(private_pointer(actual, length) && actual != value);
      assert(memcmp((void *) actual, (void *) value, length) == 0);
      memset((void *) actual, 0x5a, length);
    } else assert(actual == value);
  }
  uintptr_t environment = launch.handoff->original_envp;
  size_t envc = 0;
  while (get((void *) environment)) {
    uintptr_t value = get((void *) environment);
    uintptr_t actual = get((void *) (launch.crt_sp + 24 + envc * 8));
    size_t length = strlen((char *) value) + 1;
    assert(private_pointer(actual, length) && actual != value);
    assert(memcmp((void *) actual, (void *) value, length) == 0);
    memset((void *) actual, 0x6b, length);
    environment += 8; ++envc;
  }
  assert(envc == launch.handoff->original_envc);
  assert(memcmp(before, guest, sizeof guest) == 0);
  assert(memcmp(image_before, image, sizeof image) == 0);
  for (size_t index = 0; index < launch.crt_sp - (uintptr_t) stack; ++index)
    assert(stack[index] == 0xa5);
  for (size_t index = required; index < sizeof stack; ++index) assert(stack[index] == 0xa5);
  ++checks;
}
static void failure(enum ps_status expected)
{
  size_t measured = 0xabc;
  struct ps_launch launch, old;
  memset(&launch, 0x3c, sizeof launch); old = launch;
  memcpy(before, guest, sizeof guest); memcpy(image_before, image, sizeof image);
  assert(ps_measure(&request, &measured) == expected && measured == 0xabc);
  assert(ps_build(&request, stack, sizeof stack, &launch) == expected);
  assert(memcmp(&launch, &old, sizeof launch) == 0);
  assert(memcmp(before, guest, sizeof guest) == 0);
  assert(memcmp(image_before, image, sizeof image) == 0);
  for (size_t index = 0; index < sizeof stack; ++index) assert(stack[index] == 0xa5);
  ++checks;
}

int main(int argc, char **argv)
{
  assert(argc == 3);
  fixture(); success();
  fixture(); image[7] = 3; put(aux(23), 1, 8); success();
  fixture();
  memmove(guest + 16, guest + 32, table_end() - 32);
  put(guest, 0, 8); put(guest + 8, 0, 8); aux_offset -= 16;
  success();
  fixture();
  memmove(guest + 32, guest + 48, table_end() - 48);
  put(guest + 32, 0, 8); aux_offset -= 16;
  success();
  fixture(); request.guest_stack.size = 7; failure(PS_ARGUMENT);
  fixture(); request.guest_stack.data++; request.guest_stack.size--; failure(PS_ARGUMENT);
  fixture(); put(guest, PS_MAX_VECTOR + 1, 8); failure(PS_STACK);
  fixture(); put(guest + 8, 0, 8); failure(PS_STACK);
  fixture(); put(guest + 24, 7, 8); failure(PS_STACK);
  fixture(); put(guest + 32, UINTPTR_MAX, 8); failure(PS_STACK);
  fixture(); put(guest + 8, (uintptr_t) guest, 8); failure(PS_STACK);
  fixture(); memset(guest + 2048, 'a', sizeof guest - 2048); failure(PS_STACK);
  fixture(); put(aux(25), (uintptr_t) guest + sizeof guest - 15, 8); failure(PS_AUX);
  fixture(); put(aux(25), (uintptr_t) guest, 8); failure(PS_AUX);
  fixture(); put(aux(31), 0, 8); failure(PS_AUX);
  fixture(); put(aux(4), 55, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(6), 8192, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(7), 0, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(15) - 8, 3, 8); failure(PS_AUX);
  fixture(); put(aux(15) - 8, 1000, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(15) - 8, 38, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(15) - 8, PS_AUX_HANDOFF, 8); failure(PS_UNSUPPORTED);
  fixture(); put(aux(25) - 8, 22, 8); failure(PS_AUX);
  fixture(); request.tool_name.size--; failure(PS_ARGUMENT);
  fixture(); request.call_stack_reserve = SIZE_MAX; {
    size_t size = 0xabc; assert(ps_measure(&request, &size) == PS_OVERFLOW && size == 0xabc); ++checks;
  }
  fixture(); image[4] = 1; failure(PS_IMAGE);
  fixture(); put(image + 18, 183, 2); failure(PS_IMAGE);
  fixture(); put(image + 24, 0, 8); failure(PS_IMAGE);
  fixture(); put(image + 32, SIZE_MAX - 8, 8); failure(PS_IMAGE);
  fixture(); put(image + 32, 65, 8); failure(PS_IMAGE);
  fixture(); put(image + 56, 0xffff, 2); failure(PS_IMAGE);
  fixture(); put(image + 64 + 3 * 56, 3, 4); failure(PS_UNSUPPORTED);
  fixture(); put(image + 64 + 3 * 56, 2, 4); failure(PS_IMAGE);
  fixture(); put(image + 64 + 3 * 56 + 16, 9000, 8); failure(PS_IMAGE);
  fixture(); put(image + 4352, 1, 8); failure(PS_UNSUPPORTED);
  fixture(); put(image + 4352, 21, 8); failure(PS_IMAGE);
  fixture(); put(image + 64 + 4, 7, 4); failure(PS_IMAGE);
  fixture(); request.image_mapping_size = 1024; failure(PS_IMAGE);
  fixture(); request.image_bias++; failure(PS_IMAGE);
  fixture(); request.crt_entry_offset = 0; failure(PS_IMAGE);
  fixture(); request.crt_entry_offset = 0x2200; failure(PS_IMAGE);
  fixture(); request.crt_entry_offset = UINTPTR_MAX; failure(PS_IMAGE);
  fixture(); put(image + 64 + 32, SIZE_MAX, 8); failure(PS_IMAGE);
  fixture(); {
    size_t size = 1024 * 1024;
    unsigned char *large = aligned_alloc(16, size);
    assert(large);
    memset(large, 0, size);
    put(large, 0, 8); put(large + 8, 0, 8);
    for (size_t index = 0; index < 40000; ++index)
      put(large + 16 + index * 8, (uintptr_t) large + 700000, 8);
    memset(large + 700000, 'x', 2048);
    request.guest_stack = (struct ps_view) {large, size};
    failure(PS_STACK);
    free(large);
  }
  fixture(); {
    struct ps_launch launch, old; memset(&launch, 0x3c, sizeof launch); old = launch;
    memcpy(before, guest, sizeof guest);
    assert(ps_build(&request, stack, 65536, &launch) == PS_SPACE);
    assert(ps_build(&request, stack, sizeof stack, (void *) stack) == PS_OVERLAP);
    assert(ps_build(&request, guest, sizeof stack, &launch) == PS_OVERLAP);
    assert(ps_build(&request, stack + 1, sizeof stack - 16, &launch) == PS_ARGUMENT);
    assert(ps_measure(&request, (void *) guest) == PS_OVERLAP);
    assert(memcmp(&launch, &old, sizeof launch) == 0);
    assert(memcmp(before, guest, sizeof guest) == 0);
    for (size_t index = 0; index < sizeof stack; ++index) assert(stack[index] == 0xa5);
    checks += 5;
  }
  for (int argument = 1; argument < argc; ++argument) {
    fixture();
    int descriptor = open(argv[argument], O_RDONLY | O_CLOEXEC);
    struct stat metadata; assert(descriptor >= 0 && fstat(descriptor, &metadata) == 0);
    void *bytes = mmap(NULL, metadata.st_size, PROT_READ, MAP_PRIVATE, descriptor, 0);
    assert(bytes != MAP_FAILED);
    request.image_file = (struct ps_view) {bytes, metadata.st_size};
    request.image_mapping_size = 0x10000000;
    failure(PS_IMAGE);
    assert(munmap(bytes, metadata.st_size) == 0 && close(descriptor) == 0);
  }
  printf("%u focused controls; both actual entry-zero ELFs rejected; no CRT entry executed\n", checks);
  return 0;
}
