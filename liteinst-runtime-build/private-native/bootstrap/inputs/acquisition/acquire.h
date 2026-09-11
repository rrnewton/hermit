#ifndef MAPPED_ACQUIRE_H
#define MAPPED_ACQUIRE_H
#include <stddef.h>
#include <stdint.h>
#include "private_startup.h"

#define MA_MAPS_LIMIT (1024u * 1024u)
#define MA_IMAGE_LIMIT (512u * 1024u * 1024u)
#define MA_IO_LIMIT 4096u
#define MA_CRT_SYMBOL "pe_private_crt_entry"

enum ma_status {
  MA_OK, MA_ARGUMENT, MA_IO, MA_LIMIT, MA_PROCFS, MA_MAPS,
  MA_STACK, MA_SEALS, MA_IMAGE, MA_CRT, MA_CLEANUP
};

struct ma_owner {
  struct ps_view original_stack;
  struct ps_view runtime_file;
  uintptr_t crt_entry_offset;
  void *maps_storage;
  size_t maps_size;
  enum ma_status status;
  long primary_error;
  long cleanup_error;
};

long ma_raw(long number, long first, long second, long third,
            long fourth, long fifth, long sixth);
enum ma_status ma_stack_extent(const unsigned char *maps, size_t size,
                               uintptr_t rsp, struct ps_view *view);
enum ma_status ma_crt_symbol(struct ps_view image, uintptr_t *offset);
enum ma_status ma_acquire(uintptr_t saved_rsp, int sealed_runtime_fd,
                         struct ma_owner *owner);
enum ma_status ma_release(struct ma_owner *owner);
enum ma_status ma_seal_image(struct ps_view selected_bytes, int *owned_fd,
                            long *primary_error, long *cleanup_error);
#endif
