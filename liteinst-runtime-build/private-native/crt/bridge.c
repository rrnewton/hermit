#include "crt_context.h"
#include <sys/auxv.h>

extern uintptr_t pe_private_initial_sp __attribute__((visibility("hidden")));
extern const struct pm_crt_context *pe_private_mapper_context
    __attribute__((visibility("hidden")));
extern const unsigned char __ehdr_start[] __attribute__((visibility("hidden")));
extern void pe_private_crt_entry(void) __attribute__((visibility("hidden")));
extern void pe_private_crt_fatal(const char *, size_t)
    __attribute__((visibility("hidden"), noreturn));

static struct pc_runtime_context runtime_context;
static unsigned int runtime_phase;

#define PC_FAIL(message) pe_private_crt_fatal(message, sizeof(message) - 1)

static void pe_private_preinit(int argc, char **argv, char **envp) {
  if (runtime_phase != 0)
    PC_FAIL("private GNU CRT: repeated preinit refused\n");
  runtime_phase = 1;
  const struct ps_handoff *handoff =
      (const struct ps_handoff *)getauxval(PS_AUX_HANDOFF);
  if (pc_context_init(&runtime_context, handoff, pe_private_initial_sp,
                      argc, argv, envp) != PC_CONTEXT_OK)
    PC_FAIL("private GNU CRT: invalid preserved handoff\n");
  if (pc_mapper_context_validate(pe_private_mapper_context, handoff,
                                 (uintptr_t)__ehdr_start,
                                 (uintptr_t)pe_private_crt_entry) != PC_CONTEXT_OK)
    PC_FAIL("private GNU CRT: invalid retained mapper context\n");
  if (pe_private_prepare_runtime(pe_private_mapper_context) != 0)
    PC_FAIL("private GNU CRT: runtime preparation failed\n");
  runtime_phase = 2;
}

__attribute__((section(".preinit_array"), used))
static void (*const private_preinit)(int, char **, char **) = pe_private_preinit;

__attribute__((visibility("hidden"), noreturn))
int pe_private_runtime_main(int argc, char **argv, char **envp) {
  if (runtime_phase != 2 || argc != runtime_context.argc ||
      argv != runtime_context.argv || envp != runtime_context.envp)
    PC_FAIL("private GNU CRT: invalid main transition\n");
  runtime_phase = 3;
  pe_private_handoff_to_interpreter(pe_private_mapper_context);
  PC_FAIL("private GNU CRT: interpreter handoff returned\n");
}
