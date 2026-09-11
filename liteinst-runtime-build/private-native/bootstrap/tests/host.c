#define main frozen_builder_main
#include "stack.c"
#undef main
#include "mapper.h"
#include <asm/unistd.h>
#include <elf.h>
#include <errno.h>

static unsigned int control_count, raw_calls, protect_count;
static long fail_number = -1;
static unsigned int fail_at, unmap_failures, read_interrupts;
static long read_limit;
static _Alignas(4096) struct {
  struct pe_entry entry;
  unsigned char padding[8192 - sizeof(struct pe_entry)];
  unsigned char scratch[65536];
} retained;
long __real_ma_raw(long, long, long, long, long, long, long);

long __wrap_ma_raw(long number, long first, long second, long third,
                   long fourth, long fifth, long sixth) {
  ++raw_calls;
  assert(number != __NR_brk && number != __NR_exit_group);
  if (number == __NR_mmap) assert((fourth & MAP_FIXED) == 0);
  if (number == __NR_mprotect) {
    ++protect_count;
    assert((third & (PROT_WRITE | PROT_EXEC)) != (PROT_WRITE | PROT_EXEC));
  }
  if (number == __NR_munmap && unmap_failures) { --unmap_failures; return -EBUSY; }
  if (number == fail_number && --fail_at == 0) return -ENOMEM;
  if (number == __NR_read && read_interrupts) { --read_interrupts; return -EINTR; }
  if (number == __NR_read && read_limit && third > read_limit) third = read_limit;
  return __real_ma_raw(number, first, second, third, fourth, fifth, sixth);
}

static void ok(const char *label) { ++control_count; printf("ok: %s\n", label); }
static void reset(void) {
  raw_calls = 0; protect_count = 0; fail_number = -1; fail_at = 0;
  unmap_failures = 0; read_interrupts = 0; read_limit = 0;
}

static struct ps_view open_view(const char *path) {
  int descriptor = open(path, O_RDONLY | O_CLOEXEC);
  struct stat status; assert(descriptor >= 0 && fstat(descriptor, &status) == 0);
  void *bytes = mmap(NULL, status.st_size, PROT_READ, MAP_PRIVATE, descriptor, 0);
  assert(bytes != MAP_FAILED && close(descriptor) == 0);
  return (struct ps_view) {bytes, status.st_size};
}

static void permission(uintptr_t address, int expected) {
  FILE *file = fopen("/proc/self/maps", "r"); assert(file);
  char line[4096]; int found = 0;
  while (fgets(line, sizeof line, file)) {
    unsigned long begin, end; char perms[5];
    if (sscanf(line, "%lx-%lx %4s", &begin, &end, perms) == 3 && begin <= address && address < end) {
      int actual = (perms[0] == 'r' ? PROT_READ : 0) | (perms[1] == 'w' ? PROT_WRITE : 0) | (perms[2] == 'x' ? PROT_EXEC : 0);
      assert(actual == expected); ++found;
    }
  }
  assert(ferror(file) == 0 && fclose(file) == 0 && found == 1);
}

static void mapping_success(struct ps_view file, uintptr_t crt) {
  struct pm_plan plan; assert(pm_validate(file, crt, &plan) == PM_OK);
  struct pm_image mapped = {0}; reset();
  assert(pm_map(file, crt, &mapped) == PM_OK);
  assert(mapped.bias % plan.alignment == 0 && mapped.span == plan.span);
  assert(mapped.bias >= (uintptr_t)mapped.reservation &&
    mapped.bias + mapped.span <= (uintptr_t)mapped.reservation + mapped.reservation_size);
  assert(protect_count == 2 * plan.count);
  for (size_t index = 0; index < plan.count; ++index) {
    struct pm_segment *load = &plan.loads[index];
    size_t rounded_file = (load->address + load->filesz + 4095) & ~(size_t)4095;
    for (size_t position = load->page_begin; position < load->page_end; ++position) {
      size_t off = (load->offset & ~(size_t)4095) + position - load->page_begin;
      unsigned char expected = (load->filesz && position < rounded_file && off < file.size) ? file.data[off] : 0;
      if (position >= load->address + load->filesz && position < load->address + load->memsz) expected = 0;
      assert(*(unsigned char *)(mapped.bias + position) == expected);
    }
    int flags = PROT_READ | ((load->flags & PF_W) ? PROT_WRITE : 0) | ((load->flags & PF_X) ? PROT_EXEC : 0);
    permission(mapped.bias + load->page_begin, flags);
  }
  for (size_t page = 0; page < mapped.reservation_size; page += 4096) {
    uintptr_t address = (uintptr_t)mapped.reservation + page; int belongs = 0;
    for (size_t index = 0; index < plan.count; ++index)
      if (mapped.bias + plan.loads[index].page_begin <= address && address < mapped.bias + plan.loads[index].page_end) belongs = 1;
    if (!belongs) permission(address, PROT_NONE);
  }
  assert(pm_unmap(&mapped) == PM_OK && pm_unmap(&mapped) == PM_OK);
  ok("real reserved mapping, file-page bytes/BSS, segment permissions, NONE gaps and release");
}

static void discovery_case(const char *value, enum pm_status expected) {
  fixture();
  strcpy((char *)guest + 2400, PM_IMAGE_ENV "=");
  strcat((char *)guest + 2400, value);
  put(guest + 4 * 8, (uintptr_t)guest + 2400, 8);
  put(guest + 5 * 8, 0, 8);
  memmove(guest + 6 * 8, guest + aux_offset, table_end() - aux_offset);
  int descriptor = -9; uintptr_t base = 0x55;
  enum pm_status result = pm_discover((struct ps_view) {guest, sizeof guest}, &descriptor, &base);
  assert(result == expected);
  if (expected == PM_OK) assert(descriptor == atoi(value) && base == 0x7f000000);
  else assert(descriptor == -9 && base == 0x55);
  ok("bounded canonical descriptor discovery");
}

int main(int argc, char **argv) {
  assert(argc == 3);
  struct ps_view file = open_view(argv[1]); uintptr_t crt;
  assert(ma_crt_symbol(file, &crt) == MA_OK);
  unsigned char *sentinel = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  assert(sentinel != MAP_FAILED); memset(sentinel, 0x5a, 4096);
  mapping_success(file, crt);
  unsigned char *edited = malloc(file.size); assert(edited);
  memcpy(edited, file.data, file.size);
  Elf64_Ehdr *eh = (void *)edited;
  Elf64_Phdr *ph = (void *)(edited + eh->e_phoff);
  size_t load = 0, note = 0, relro = 0;
  for (size_t index = 0; index < eh->e_phnum; ++index) {
    if (ph[index].p_type == PT_LOAD && ph[index].p_flags == (PF_R | PF_W)) load = index;
    if (ph[index].p_type == PT_NOTE) note = index;
    if (ph[index].p_type == PT_GNU_RELRO) relro = index;
  }
  assert(load && note && relro);
  ph[0].p_align = PM_MAX_ALIGN; mapping_success((struct ps_view){edited, file.size}, crt);
  memcpy(edited, file.data, file.size); ph[load].p_flags |= PF_X;
  struct pm_image bad = {0}; reset();
  assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_UNSUPPORTED && raw_calls == 0);
  ok("RWX refused before effects");
  memcpy(edited, file.data, file.size); ph[load].p_memsz = SIZE_MAX;
  reset(); assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_FORMAT && raw_calls == 0);
  ok("overflow refused before effects");
  memcpy(edited, file.data, file.size);
  ph[note] = ph[load]; ph[note].p_vaddr = (ph[load].p_vaddr & ~(uint64_t)4095) + 4095;
  ph[note].p_offset = 4095; ph[note].p_filesz = 1; ph[note].p_memsz = 2;
  reset(); assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_OVERLAP && raw_calls == 0);
  ok("partial page/load intersection refused before effects");
  memcpy(edited, file.data, file.size); ph[note] = ph[relro];
  reset(); assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_FORMAT && raw_calls == 0);
  ok("duplicate RELRO refused");
  memcpy(edited, file.data, file.size); ph[0].p_align = PM_MAX_ALIGN * 2;
  reset(); assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_UNSUPPORTED && raw_calls == 0);
  ok("unsupported alignment refused before effects");
  memcpy(edited, file.data, file.size); ph[load].p_offset += 1;
  reset(); assert(pm_map((struct ps_view){edited, file.size}, crt, &bad) == PM_FORMAT && raw_calls == 0);
  ok("file/memory page incongruence refused before effects");
  reset(); fail_number = __NR_mmap; fail_at = 1;
  assert(pm_map(file, crt, &bad) == PM_IO && bad.primary_error == -ENOMEM && !bad.reservation);
  ok("reservation failure retained");
  reset(); fail_number = __NR_mprotect; fail_at = 1; unmap_failures = 1;
  assert(pm_map(file, crt, &bad) == PM_IO && bad.primary_error == -ENOMEM && bad.cleanup_error == -EBUSY && bad.reservation);
  reset(); assert(pm_unmap(&bad) == PM_OK);
  ok("failed protection and cleanup retain owned range for retry");
  reset(); fail_number = __NR_mprotect; fail_at = 2;
  assert(pm_map(file, crt, &bad) == PM_IO && !bad.reservation);
  ok("final protection failure releases initialized mapping");
  for (size_t index = 0; index < 4096; ++index) assert(sentinel[index] == 0x5a);
  assert(munmap(sentinel, 4096) == 0); ok("unrelated sentinel unchanged");
  reset();
  discovery_case("3", PM_OK); discovery_case("2147483647", PM_OK);
  for (size_t index = 0; index < 8; ++index) {
    const char *values[] = {"", "0", "2", "03", "+3", "-3", "3 ", "2147483648"};
    discovery_case(values[index], PM_DISCOVERY);
  }
  fixture(); strcpy((char *)guest + 2120, PM_IMAGE_ENV "=3");
  strcpy((char *)guest + 2160, PM_IMAGE_ENV "=4");
  int selected = -1; uintptr_t base = 0;
  assert(pm_discover((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  ok("duplicate discovery refused");
  fixture(); selected = -1; base = 0;
  assert(pm_discover((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  assert(selected == -1 && base == 0); ok("missing descriptor leaves outputs untouched");
  fixture(); strcpy((char *)guest + 2120, PM_IMAGE_ENV "=3");
  put(guest + 5 * 8, (uintptr_t)guest + sizeof guest, 8);
  assert(pm_discover((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  ok("out-of-bounds environment pointer refused");
  fixture(); strcpy((char *)guest + 2120, PM_IMAGE_ENV "=3");
  put(aux(7), 0, 8);
  assert(pm_discover((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  ok("zero original AT_BASE refused");
  fixture(); strcpy((char *)guest + 2400, PM_IMAGE_ENV "=3");
  strcpy((char *)guest + 2500, PM_ORIGINAL_ENV "=4");
  put(guest + 4 * 8, (uintptr_t)guest + 2400, 8);
  put(guest + 5 * 8, (uintptr_t)guest + 2500, 8);
  assert(pm_discover_original((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_OK);
  assert(selected == 4 && base == 0x7f000000);
  assert(pm_discover((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_OK && selected == 3);
  ok("runtime and original interpreter descriptors are independently discovered");
  strcpy((char *)guest + 2400, PM_ORIGINAL_ENV "=5");
  assert(pm_discover_original((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  ok("duplicate original descriptor refused");
  strcpy((char *)guest + 2400, PM_IMAGE_ENV "=3");
  strcpy((char *)guest + 2500, PM_ORIGINAL_ENV "=04");
  assert(pm_discover_original((struct ps_view){guest, sizeof guest}, &selected, &base) == PM_DISCOVERY);
  ok("noncanonical original descriptor refused");
  struct ps_view actual = open_view(argv[2]);
  assert(pm_map(actual, 1, &bad) == PM_FORMAT); ok("actual unbound full Detcore image remains refused");
  assert(munmap((void *)actual.data, actual.size) == 0);
  fixture();
  void *guarded = mmap(NULL, 12288, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  assert(guarded != MAP_FAILED);
  unsigned char *initial = (unsigned char *)guarded + 4096;
  assert(mprotect(initial, 4096, PROT_READ | PROT_WRITE) == 0); memcpy(initial, guest, 4096);
  for (size_t offset = 0; offset < table_end(); offset += 8) {
    uint64_t value = get(initial + offset);
    if ((uintptr_t)guest <= value && value < (uintptr_t)guest + sizeof guest)
      put(initial + offset, (uintptr_t)initial + value - (uintptr_t)guest, 8);
  }
  memset(&retained, 0, sizeof retained);
  struct pe_entry *entry = &retained.entry;
  entry->version = PE_VERSION; entry->size = sizeof *entry; entry->valid = PE_VALID_ALL;
  entry->rsp = (uintptr_t)initial; entry->xcr0 = entry->cpu_user_mask = 0x2e7; entry->xstate_size = 2440;
  entry->storage_begin = (uintptr_t)entry; entry->scratch_begin = (uintptr_t)retained.scratch;
  entry->storage_end = entry->scratch_end = (uintptr_t)retained.scratch + sizeof retained.scratch;
  memset(entry->xstate, 0x69, sizeof entry->xstate);
  struct ma_owner checked_maps = {0}; size_t checked_size = 99;
  reset(); read_limit = 31; read_interrupts = 2;
  assert(pm_read_maps(entry->rsp, &checked_maps, &checked_size) == MA_OK && checked_size > 99);
  assert(checked_maps.original_stack.data == initial && checked_maps.original_stack.size == 4096);
  assert(ma_release(&checked_maps) == MA_OK);
  ok("actual maps read handles short reads/EINTR and finds only guarded readable page");
  reset(); fail_number = __NR_read; fail_at = 1; unmap_failures = 1; checked_size = 99;
  assert(pm_read_maps(entry->rsp, &checked_maps, &checked_size) == MA_IO);
  assert(checked_maps.primary_error == -ENOMEM && checked_maps.cleanup_error == -EBUSY &&
    checked_maps.maps_storage != NULL && checked_size == 99);
  reset(); assert(ma_release(&checked_maps) == MA_OK);
  ok("maps read primary and cleanup errors retained with retryable ownership");
  int descriptor = -1, other = -1; long primary = 0, cleanup = 0;
  assert(ma_seal_image(file, &descriptor, &primary, &cleanup) == MA_OK);
  assert(ma_seal_image(file, &other, &primary, &cleanup) == MA_OK);
  struct pm_plan plan; assert(pm_validate(file, crt, &plan) == PM_OK);
  void *kernel_fixture = mmap(NULL, plan.span, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  assert(kernel_fixture != MAP_FAILED);
  for (size_t index = 0; index < plan.count; ++index) {
    struct pm_segment *part = &plan.loads[index];
    size_t length = ((part->address + part->filesz + 4095) & ~(size_t)4095) - part->page_begin;
    if (part->filesz) assert(mmap((unsigned char *)kernel_fixture + part->page_begin, length, PROT_READ,
        MAP_PRIVATE | MAP_FIXED, descriptor, part->offset & ~(size_t)4095) == (unsigned char *)kernel_fixture + part->page_begin);
  }
  entry->entry_pc = (uintptr_t)kernel_fixture + plan.kernel_entry;
  struct ma_owner inventory = {0}; size_t inventory_size;
  assert(pm_read_maps(entry->rsp, &inventory, &inventory_size) == MA_OK);
  struct ps_view inventory_view = {inventory.maps_storage, inventory_size};
  assert(pm_check_binding(entry, descriptor, file, crt, (uintptr_t)kernel_fixture, inventory_view) == PM_OK);
  assert(pm_check_binding(entry, other, file, crt, (uintptr_t)kernel_fixture, inventory_view) == PM_BINDING);
  ok("real file mappings match same sealed inode, not resealed identical bytes");
  ++entry->entry_pc;
  assert(pm_check_binding(entry, descriptor, file, crt, (uintptr_t)kernel_fixture, inventory_view) == PM_BINDING);
  --entry->entry_pc;
  ok("different actual entry offset refused");
  assert(ma_release(&inventory) == MA_OK && munmap(kernel_fixture, plan.span) == 0 && close(other) == 0);
  struct ma_owner acquired = {0};
  assert(ma_acquire(entry->rsp, descriptor, &acquired) == MA_OK && acquired.original_stack.size == 4096);
  struct pm_owner *owner = aligned_alloc(_Alignof(struct pm_owner), sizeof(struct pm_owner)); assert(owner);
  memset(owner, 0, sizeof *owner); owner->runtime_fd = descriptor;
  owner->original_brk = 0x12345000;
  unsigned char original[4096]; memcpy(original, initial, 4096);
  assert(pm_prepare(entry, &acquired, owner) == PM_OK && acquired.runtime_file.data == NULL);
  assert(owner->original_brk == 0x12345000);
  assert(memcmp(&owner->captured, entry, sizeof *entry) == 0 && memcmp(original, initial, 4096) == 0);
  assert(owner->context.captured == &owner->captured && owner->context.stack_handoff == owner->launch.handoff);
  assert(owner->launch.crt_entry == owner->image.bias + crt && owner->launch.crt_rdx == 0 && owner->launch.crt_sp % 16 == 0);
  assert(owner->launch.handoff->original_sp == (uintptr_t)initial);
  owner->transferred = 1; assert(pm_release(owner) == PM_ARGUMENT); owner->transferred = 0;
  unmap_failures = 1;
  assert(pm_release(owner) == PM_CLEANUP && owner->stack != NULL && owner->cleanup_error == -EBUSY);
  assert(pm_release(owner) == PM_OK && pm_release(owner) == PM_OK);
  assert(fcntl(descriptor, F_GETFD) >= 0 && memcmp(original, initial, 4096) == 0);
  ok("real mapper and frozen builder prepare owned stack and exact synthetic state copy; no transfer");
  memset(owner, 0, sizeof *owner);
  owner->original_brk = 0x12345000;
  assert(ma_acquire(entry->rsp, descriptor, &acquired) == MA_OK);
  reset(); fail_number = __NR_mmap; fail_at = 2;
  assert(pm_prepare(entry, &acquired, owner) == PM_IO && owner->primary_error == -ENOMEM);
  assert(owner->original_brk == 0x12345000);
  assert(acquired.runtime_file.data == NULL && owner->acquired.runtime_file.data == NULL &&
    owner->image.reservation == NULL && owner->stack == NULL);
  assert(memcmp(original, initial, 4096) == 0 && memcmp(&owner->captured, entry, sizeof *entry) == 0);
  assert(fcntl(descriptor, F_GETFD) >= 0);
  ok("CRT-stack allocation failure releases only owned storage and preserves original state/borrowed fd");
  free(owner); assert(close(descriptor) == 0 && munmap(guarded, 12288) == 0);
  free(edited); assert(munmap((void *)file.data, file.size) == 0);
  printf("%u host mapping/preparation controls; no mapped instruction, entry, CRT or handoff executed\n", control_count);
  return 0;
}
