/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

enum { MAPPING_COUNT = 4 };

int main(int argc, char** argv) {
  int perturb_third = argc == 2 && strcmp(argv[1], "perturb-third") == 0;
  if (argc > 2 || (argc == 2 && !perturb_third)) {
    fprintf(stderr, "usage: %s [perturb-third]\n", argv[0]);
    return 2;
  }
  long raw_page_size = sysconf(_SC_PAGESIZE);
  if (raw_page_size <= 0) {
    perror("sysconf");
    return 1;
  }
  size_t page_size = (size_t)raw_page_size;
  void* mappings[MAPPING_COUNT];

  for (size_t index = 0; index < MAPPING_COUNT; ++index) {
    size_t length = (index + 1) * page_size;
    mappings[index] = mmap(
        NULL,
        length,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0);
    if (mappings[index] == MAP_FAILED) {
      perror("mmap");
      return 1;
    }
    *((volatile unsigned char*)mappings[index]) = (unsigned char)index;
  }

  /*
   * Test-only negative control: change one backend's allocator history through a real
   * guest syscall. Growing the third mapping with MREMAP_MAYMOVE makes Detcore allocate a
   * new canonical address for that mapping, while the other three reported mappings stay
   * put. The Rust parity test passes this argument only to its DBI arm and requires the
   * exact comparator to report 3/4 matches and index 2 as the difference.
   */
  if (perturb_third) {
    void* moved = mremap(
        mappings[2], 3 * page_size, 4 * page_size, MREMAP_MAYMOVE);
    if (moved == MAP_FAILED) {
      perror("mremap");
      return 1;
    }
    mappings[2] = moved;
  }

  printf("mmap-addresses count=%d", MAPPING_COUNT);
  for (size_t index = 0; index < MAPPING_COUNT; ++index) {
    printf(" 0x%" PRIxPTR, (uintptr_t)mappings[index]);
  }
  putchar('\n');
  return 0;
}
