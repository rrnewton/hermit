#ifndef PRIVATE_MAPPER_H
#define PRIVATE_MAPPER_H
#include "acquire.h"
#include "entry.h"
#include "private_crt_context.h"

#define PM_VERSION PM_CONTEXT_VERSION
#define PM_PAGE 4096u
#define PM_MAX_SPAN (512u * 1024u * 1024u)
#define PM_MAX_ALIGN (2u * 1024u * 1024u)
#define PM_IMAGE_ENV "HERMIT_LITEINST_PRIVATE_RUNTIME_FD"
#define PM_ORIGINAL_ENV "HERMIT_LITEINST_ORIGINAL_INTERPRETER_FD"
#define PM_BINDING_ENV "HERMIT_LITEINST_INTERPRETER_BINDING_FD"
#define PM_BINDING_HEADER 208u
#define PM_BINDING_LIMIT (PM_BINDING_HEADER + 4096u)

enum pm_status {
  PM_OK, PM_ARGUMENT, PM_FORMAT, PM_UNSUPPORTED, PM_OVERLAP,
  PM_IO, PM_DISCOVERY, PM_ACQUIRE, PM_BINDING, PM_BUILDER, PM_CLEANUP
};

struct pm_segment {
  uintptr_t address, offset;
  size_t filesz, memsz, page_begin, page_end;
  unsigned int flags;
};

struct pm_plan {
  size_t span, alignment, count;
  uintptr_t kernel_entry, crt_entry;
  size_t relro_begin, relro_end;
  struct pm_segment loads[128];
};

struct pm_image {
  void *reservation;
  size_t reservation_size;
  uintptr_t bias;
  size_t span, relro_begin, relro_end;
  long primary_error, cleanup_error;
  enum pm_status status;
};

struct pm_owner {
  struct pm_crt_context context;
  struct pe_entry captured;
  const struct pe_entry *origin;
  struct ma_owner acquired, maps;
  struct pm_image image;
  void *stack;
  size_t stack_size;
  struct ps_launch launch;
  int runtime_fd, original_fd, transferred, restoration_complete;
  enum pm_status status;
  enum ma_status acquisition_status;
  enum ps_status builder_status;
  long primary_error, cleanup_error;
  uintptr_t original_brk;
};

enum pm_status pm_validate(struct ps_view file, uintptr_t crt, struct pm_plan *plan);
long pm_read_initial_brk(uintptr_t *output);
enum pm_status pm_map(struct ps_view file, uintptr_t crt, struct pm_image *image);
enum pm_status pm_unmap(struct pm_image *image);
enum ma_status pm_read_maps(uintptr_t rsp, struct ma_owner *maps, size_t *used);
enum pm_status pm_discover(struct ps_view original_stack, int *fd, uintptr_t *at_base);
enum pm_status pm_discover_original(struct ps_view original_stack, int *fd, uintptr_t *at_base);
enum pm_status pm_discover_restoration(struct ps_view original_stack, int *fd, uintptr_t *at_base);
enum pm_status pm_restore_interpreter_binding(const struct pe_entry *entry, struct pm_owner *owner,
                                            const struct ma_owner *input, uintptr_t at_base, size_t maps_size);
enum pm_status pm_check_binding(const struct pe_entry *entry, int fd,
                               struct ps_view file, uintptr_t crt, uintptr_t at_base,
                               struct ps_view maps);
enum pm_status pm_prepare(const struct pe_entry *entry, struct ma_owner *acquired,
                         struct pm_owner *owner);
enum pm_status pm_release(struct pm_owner *owner);

#endif
