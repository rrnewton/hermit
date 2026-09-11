#include "private_tls.h"
#include "mapper.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>

int main(void) {
  const uint64_t private_fs = 0x70000;
  const uint64_t guest_fs = 0x80000;
  assert(pl_fs_role(private_fs, guest_fs, guest_fs) == 2);
  assert(pl_fs_role(private_fs, guest_fs, guest_fs + 8) == 0);
  assert(pl_fs_role(private_fs, guest_fs, private_fs) == 1);
  assert(pl_fs_role(private_fs, guest_fs, 0) == 0);
  assert(pl_fs_role(private_fs, 0, 0) == 2);
  assert(pl_fs_role(private_fs, 0, guest_fs) == 0);
  assert(pl_fs_role(private_fs, private_fs, private_fs) == 0);
  assert(pl_fs_role(0, guest_fs, guest_fs) == 0);
  assert(pl_fs_role(0, 0, 0) == 0);
  struct pm_plan plan = {0};
  assert(pl_file_prefix(NULL, 4096) == -EINVAL);
  assert(pl_file_prefix(&plan, 0) == -EINVAL);
  assert(pl_file_prefix(&plan, 1) == -EINVAL);
  assert(pl_file_prefix(&plan, 4096) == -ENOTSUP);
  plan.count = 2;
  plan.loads[0] = (struct pm_segment) {.address = 4096, .filesz = 1, .page_begin = 4096};
  plan.loads[1] = (struct pm_segment) {.address = 0, .filesz = 4096, .page_begin = 0};
  assert(pl_file_prefix(&plan, 8192) == 0);
  assert(pl_file_prefix(&plan, 12288) == -ENOTSUP);
  plan.loads[0].filesz = 0;
  plan.loads[0].memsz = 8192;
  assert(pl_file_prefix(&plan, 8192) == -ENOTSUP);
  plan.loads[0].filesz = 1;
  plan.loads[0].address = 8192;
  plan.loads[0].page_begin = 8192;
  assert(pl_file_prefix(&plan, 12288) == -ENOTSUP);
  plan.loads[0].address = UINTPTR_MAX;
  assert(pl_file_prefix(&plan, 8192) == -EINVAL);
  plan.count = 129;
  assert(pl_file_prefix(&plan, 8192) == -EINVAL);

  struct pm_owner owner = {0};
  owner.image.reservation = (void *)0x20000;
  owner.image.reservation_size = 4096;
  owner.stack = (void *)0x30000;
  owner.stack_size = 4096;
  owner.acquired.original_stack = (struct ps_view) {(void *)0x40000, 4096};
  owner.acquired.runtime_file = (struct ps_view) {(void *)0x50000, 4096};
  assert(pl_destination_disjoint(&owner, 0x10000, 4096) == 0);
  assert(pl_destination_disjoint(&owner, 0x1f000, 4096) == 0);
  assert(pl_destination_disjoint(&owner, 0x1f000, 8192) == -EPERM);
  assert(pl_destination_disjoint(&owner, 0x20000, 4096) == -EPERM);
  assert(pl_destination_disjoint(&owner, 0x30000, 4096) == -EPERM);
  assert(pl_destination_disjoint(&owner, 0x40000, 4096) == -EPERM);
  assert(pl_destination_disjoint(&owner, 0x50000, 4096) == -EPERM);
  assert(pl_destination_disjoint(&owner, (uintptr_t)&owner & ~(uintptr_t)4095, 8192) == -EPERM);
  assert(pl_destination_disjoint(&owner, UINTPTR_MAX & ~(uintptr_t)4095, 8192) == -EINVAL);
  owner.stack_size = 0;
  assert(pl_destination_disjoint(&owner, 0x10000, 4096) == -EPERM);

  struct pl_initial_inputs inputs;
  assert(pl_initial_inputs(NULL, &inputs) == -EINVAL);
  assert(pl_initial_inputs(&owner.context, NULL) == -EINVAL);
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  unsigned char original_stack[128] = {0};
  struct ps_handoff handoff = {
    .version = PS_ABI_VERSION, .size = sizeof handoff,
    .original_sp = (uintptr_t)original_stack,
    .original_stack_end = (uintptr_t)original_stack + sizeof original_stack,
    .original_auxv = (uintptr_t)original_stack + 16, .original_auxc = 2
  };
  owner.captured.rsp = handoff.original_sp;
  owner.acquired.original_stack = (struct ps_view) {original_stack, sizeof original_stack};
  owner.context = (struct pm_crt_context) {
    .version = PM_CONTEXT_VERSION, .size = sizeof owner.context,
    .owner = &owner, .captured = &owner.captured, .stack_handoff = &handoff
  };
  owner.launch.handoff = &handoff;
  owner.transferred = 1;
  owner.status = PM_OK;
  owner.original_fd = 3;
  owner.runtime_fd = 4;
  owner.original_brk = 0x12345000;
  assert(pl_initial_inputs(&owner.context, &inputs) == 0);
  assert(inputs.auxv == original_stack + 16 && inputs.auxv_bytes == 48 &&
         inputs.stack_begin == handoff.original_sp &&
         inputs.stack_end == handoff.original_stack_end && inputs.original_fd == 3 &&
         inputs.original_brk == owner.original_brk);
  owner.original_brk = 0;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  assert(inputs.original_brk == 0x12345000);
  owner.original_brk = UINT64_C(1) << 47;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  assert(inputs.original_brk == 0x12345000);
  owner.original_brk = 0x12345000;
  struct pm_crt_context copy = owner.context;
  assert(pl_initial_inputs(&copy, &inputs) == -EINVAL);
  owner.original_fd = 4;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  owner.original_fd = 3;
  owner.transferred = 0;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  owner.transferred = 1;
  handoff.original_auxv = handoff.original_stack_end - 16;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  handoff.original_auxc = 0;
  assert(pl_initial_inputs(&owner.context, &inputs) == 0 && inputs.auxv_bytes == 16);
  handoff.original_auxv = handoff.original_stack_end;
  assert(pl_initial_inputs(&owner.context, &inputs) == -EINVAL);
  puts("31 prefix/ownership/input + 9 FS identity + 4 retained-brk controls; no TLS, mapping, entry or CRT execution");
  return 0;
}
