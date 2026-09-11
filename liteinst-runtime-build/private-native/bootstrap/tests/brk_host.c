#include "mapper.h"
#include <asm/unistd.h>
#include <assert.h>
#include <errno.h>
#include <stdio.h>

static long injected;
static unsigned calls;
static int native_query;
extern long __real_ma_raw(long, long, long, long, long, long, long);

long __wrap_ma_raw(long number, long first, long second, long third,
                   long fourth, long fifth, long sixth) {
  assert(number == __NR_brk);
  assert(first == 0 && second == 0 && third == 0 && fourth == 0 && fifth == 0 && sixth == 0);
  ++calls;
  if (native_query) return __real_ma_raw(number, first, second, third, fourth, fifth, sixth);
  printf("injected raw brk(0,0,0,0,0,0) -> %ld\n", injected);
  return injected;
}

int main(void) {
  uintptr_t output = 0x55aa;
  assert(pm_read_initial_brk(NULL) == -EINVAL && calls == 0);
  const long results[] = {1, 0x12345000, 0x12345000, 0x12345001, (1L << 47) - 1,
                          0, -ENOMEM, -EPERM, 1L << 47};
  for (size_t index = 0; index < sizeof results / sizeof results[0]; ++index) {
    injected = results[index];
    output = 0x55aa;
    unsigned before = calls;
    long result = pm_read_initial_brk(&output);
    assert(calls == before + 1);
    if (injected > 0 && injected < (1L << 47)) {
      assert(result == 0 && output == (uintptr_t)injected);
    } else {
      assert(result == (injected < 0 ? injected : -EINVAL));
      assert(output == 0x55aa);
    }
  }
  native_query = 1;
  assert(pm_read_initial_brk(&output) == 0);
  long current = __real_ma_raw(__NR_brk, 0, 0, 0, 0, 0, 0);
  assert(current > 0 && output == (uintptr_t)current);
  assert(calls == 10);
  puts("11 provider cases: null, 9 injected results, ordinary-host query; no brk setter or CRT entry");
  return 0;
}
