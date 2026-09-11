#ifndef PRIVATE_INITIAL_TRANSFER_H
#define PRIVATE_INITIAL_TRANSFER_H
#include "private_tls.h"
#include "entry.h"
struct pl_initial_record {
  const struct pe_entry *capture;
  uintptr_t loader_entry;
  uintptr_t fault_site;
};
_Static_assert(sizeof(struct pl_initial_record) == 24, "initial record size");
__attribute__((visibility("hidden")))
int pl_arm_initial(const struct pm_crt_context *context, uintptr_t loader_entry);
__attribute__((visibility("hidden")))
const struct pl_initial_record *pl_take_initial(void);
__attribute__((visibility("hidden"), noreturn))
void pl_restore_and_fault(void);
#endif
