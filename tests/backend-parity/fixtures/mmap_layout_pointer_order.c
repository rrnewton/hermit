/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

enum { MAPPING_COUNT = 4 };

static size_t page_size(void) {
  long raw_page_size = sysconf(_SC_PAGESIZE);
  if (raw_page_size <= 0) {
    perror("sysconf");
    exit(1);
  }
  return (size_t)raw_page_size;
}

static int run_layout(int perturb_third) {
  size_t page = page_size();
  void* mappings[MAPPING_COUNT];

  for (size_t index = 0; index < MAPPING_COUNT; ++index) {
    size_t length = (index + 1) * page;
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
    void* moved =
        mremap(mappings[2], 3 * page, 4 * page, MREMAP_MAYMOVE);
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

static int run_growdown(void) {
  size_t page = page_size();
  unsigned char* mapping = mmap(
      NULL,
      page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN,
      -1,
      0);
  if (mapping == MAP_FAILED) {
    perror("growdown mmap");
    return 1;
  }
  mapping[0] = 0x31;
  volatile unsigned char* expanded = mapping - page;
  *expanded = 0x7a;
  if (mapping[0] != 0x31 || *expanded != 0x7a) {
    fprintf(stderr, "growdown expansion lost data\n");
    return 1;
  }
  puts("growdown-expansion-ok");
  return 0;
}

#ifndef MAP_HUGE_SHIFT
#define MAP_HUGE_SHIFT 26
#endif
#ifndef MAP_HUGE_2MB
#define MAP_HUGE_2MB (21 << MAP_HUGE_SHIFT)
#endif

static int try_hugetlb(int extra_flags, const char* label) {
  size_t huge_size = 2 * 1024 * 1024;
  errno = 0;
  void* mapping = mmap(
      NULL,
      huge_size,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB | extra_flags,
      -1,
      0);
  if (mapping == MAP_FAILED) {
    if (errno == EINVAL) {
      fprintf(stderr, "%s hugetlb request was converted to EINVAL\n", label);
      return 1;
    }
    return 0;
  }
  if (extra_flags == MAP_HUGE_2MB && (uintptr_t)mapping % huge_size != 0) {
    fprintf(stderr, "explicit 2MiB hugetlb mapping is misaligned: %p\n", mapping);
    return 1;
  }
  if (munmap(mapping, huge_size) != 0) {
    perror("hugetlb munmap");
    return 1;
  }
  return 0;
}

static int run_hugetlb(void) {
  if (try_hugetlb(0, "default") != 0 ||
      try_hugetlb(MAP_HUGE_2MB, "explicit-2MiB") != 0) {
    return 1;
  }
  puts("hugetlb-semantics-preserved");
  return 0;
}

static int run_untracked_collision(void) {
  size_t page = page_size();
  unsigned char* probe = mmap(
      NULL,
      page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (probe == MAP_FAILED || munmap(probe, page) != 0) {
    perror("canonical probe mmap");
    return 1;
  }
  unsigned char* growdown = probe;
  void* mapped = mmap(
      growdown,
      page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN | MAP_FIXED_NOREPLACE,
      -1,
      0);
  if (mapped != growdown) {
    perror("fixed growdown mmap");
    return 1;
  }
  growdown[0] = 0x41;
  volatile unsigned char* hidden = growdown - page;
  *hidden = 0x52;

  unsigned char* source = mmap(
      (void*)UINT64_C(0x300000000000),
      page,
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
      -1,
      0);
  if (source == MAP_FAILED) {
    perror("mremap source mmap");
    return 1;
  }
  source[0] = 0x63;
  errno = 0;
  void* moved = mremap(source, page, 2 * page, MREMAP_MAYMOVE);
  if (moved != MAP_FAILED || errno != EEXIST) {
    fprintf(
        stderr,
        "untracked collision was not refused: moved=%p errno=%d\n",
        moved,
        errno);
    return 1;
  }
  if (source[0] != 0x63 || *hidden != 0x52 || growdown[0] != 0x41) {
    fprintf(stderr, "collision refusal clobbered a live mapping\n");
    return 1;
  }
  puts("untracked-collision-refused-no-clobber");
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 1) {
    return run_layout(0);
  }
  if (argc != 2) {
    fprintf(stderr, "usage: %s [perturb-third|growdown|hugetlb|collision]\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "perturb-third") == 0) {
    return run_layout(1);
  }
  if (strcmp(argv[1], "growdown") == 0) {
    return run_growdown();
  }
  if (strcmp(argv[1], "hugetlb") == 0) {
    return run_hugetlb();
  }
  if (strcmp(argv[1], "collision") == 0) {
    return run_untracked_collision();
  }
  fprintf(stderr, "unknown mode: %s\n", argv[1]);
  return 2;
}
