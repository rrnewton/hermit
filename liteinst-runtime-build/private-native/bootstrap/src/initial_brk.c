#include "mapper.h"
#include <asm/unistd.h>
#include <errno.h>

long pm_read_initial_brk(uintptr_t *output) {
  if (output == NULL) return -EINVAL;
  long observed = ma_raw(__NR_brk, 0, 0, 0, 0, 0, 0);
  if (observed < 0) return observed;
  if (observed == 0 || (unsigned long)observed >= (UINT64_C(1) << 47)) return -EINVAL;
  *output = (uintptr_t)observed;
  return 0;
}
