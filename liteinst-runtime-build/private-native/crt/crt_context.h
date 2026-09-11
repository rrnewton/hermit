#ifndef PC_CRT_CONTEXT_H
#define PC_CRT_CONTEXT_H

#include "mapper.h"

struct pc_runtime_context {
  struct ps_handoff handoff;
  uintptr_t private_initial_sp;
  int argc;
  char **argv;
  char **envp;
};

enum pc_context_status {
  PC_CONTEXT_OK,
  PC_CONTEXT_ARGUMENT,
  PC_CONTEXT_VERSION,
  PC_CONTEXT_PRIVATE_STACK,
  PC_CONTEXT_ORIGINAL_STACK,
  PC_CONTEXT_OVERLAP,
  PC_CONTEXT_MAPPER,
  PC_CONTEXT_CAPTURE,
  PC_CONTEXT_IMAGE
};

__attribute__((visibility("hidden")))
enum pc_context_status pc_context_init(struct pc_runtime_context *,
                                     const struct ps_handoff *, uintptr_t,
                                     int, char **, char **);

__attribute__((visibility("hidden")))
enum pc_context_status pc_mapper_context_validate(const struct pm_crt_context *,
                                                 const struct ps_handoff *,
                                                 uintptr_t, uintptr_t);

__attribute__((visibility("hidden")))
int pe_private_prepare_runtime(const struct pm_crt_context *);

__attribute__((visibility("hidden")))
void pe_private_handoff_to_interpreter(const struct pm_crt_context *);

#endif
