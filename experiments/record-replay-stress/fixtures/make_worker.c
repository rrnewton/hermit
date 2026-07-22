#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

enum { ITERATIONS = 250000 };

static uint64_t mix(int worker) {
  uint64_t state = (uint64_t)worker + 1;
  for (int iteration = 1; iteration <= ITERATIONS; iteration++) {
    state =
        (state ^ (uint64_t)iteration) * UINT64_C(1103515245) + UINT64_C(12345);
  }
  return state;
}

int main(int argc, char **argv) {
  char *end = NULL;
  errno = 0;
  long worker = argc == 2 ? strtol(argv[1], &end, 10) : -1;
  if (errno != 0 || end == NULL || *end != '\0' || worker < 0 || worker > 3) {
    return 2;
  }
  printf("built=%ld checksum=%" PRIu64 "\n", worker, mix((int)worker));
  return 0;
}
