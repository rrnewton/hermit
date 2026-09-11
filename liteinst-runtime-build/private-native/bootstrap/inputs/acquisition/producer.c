#define _GNU_SOURCE
#include "acquire.h"
#include <asm/unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/memfd.h>

enum ma_status ma_seal_image(struct ps_view selected_bytes, int *owned_fd,
                            long *primary_error, long *cleanup_error) {
  if (owned_fd == NULL || primary_error == NULL || cleanup_error == NULL)
    return MA_ARGUMENT;
  uintptr_t crt;
  enum ma_status status = ma_crt_symbol(selected_bytes, &crt);
  if (status != MA_OK) return status;
  *primary_error = 0; *cleanup_error = 0;
  long result = ma_raw(__NR_memfd_create, (long)"hermit-private-crt-input",
                       MFD_CLOEXEC | MFD_ALLOW_SEALING, 0, 0, 0, 0);
  if (result < 0) { *primary_error = result; return MA_IO; }
  int descriptor = (int)result;
  size_t written = 0;
  for (unsigned int attempts = 0; written < selected_bytes.size &&
       attempts < MA_IO_LIMIT; ++attempts) {
    result = ma_raw(__NR_pwrite64, descriptor, (long)(selected_bytes.data + written),
                    (long)(selected_bytes.size - written), (long)written, 0, 0);
    if (result == -EINTR) continue;
    if (result <= 0 || (size_t)result > selected_bytes.size - written) {
      status = MA_IO; *primary_error = result; goto failure;
    }
    written += (size_t)result;
  }
  if (written != selected_bytes.size) { status = MA_LIMIT; goto failure; }
  result = ma_raw(__NR_fcntl, descriptor, F_ADD_SEALS,
                  F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE, 0, 0, 0);
  if (result < 0) { status = MA_SEALS; *primary_error = result; goto failure; }
  *owned_fd = descriptor;
  return MA_OK;
failure:
  result = ma_raw(__NR_close, descriptor, 0, 0, 0, 0, 0);
  if (result < 0) *cleanup_error = result;
  return status;
}
