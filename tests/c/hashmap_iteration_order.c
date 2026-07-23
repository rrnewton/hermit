/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Hash-table iteration order nondeterminism. Like Python's dict, Rust's
 * HashMap, and many other runtimes, this hash set mixes a per-process random
 * seed (from getrandom) into the hash function to resist algorithmic-complexity
 * attacks. The stored keys are fixed, but the seed decides which bucket each
 * lands in, so the iteration order changes from run to run natively.
 *
 * Hermit determinizes the getrandom seed, so the iteration order is identical
 * on every run.
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>

enum { CAPACITY = 16 };

static const char* const KEYS[] = {
    "apple", "banana", "cherry",     "date", "elderberry",
    "fig",   "grape",  "honeydew",   "kiwi", "lemon",
};
enum { KEY_COUNT = sizeof(KEYS) / sizeof(KEYS[0]) };

/* FNV-1a mixed with a random seed so the bucket layout is seed-dependent. */
static size_t hash_key(const char* key, uint64_t seed) {
  uint64_t hash = 1469598103934665603ULL ^ seed;
  for (const unsigned char* byte = (const unsigned char*)key; *byte; byte++) {
    hash ^= *byte;
    hash *= 1099511628211ULL;
  }
  return (size_t)(hash % CAPACITY);
}

int main(void) {
  uint64_t seed = 0;
  if (getrandom(&seed, sizeof(seed), 0) != (ssize_t)sizeof(seed)) {
    return 1;
  }

  const char* buckets[CAPACITY];
  for (size_t i = 0; i < CAPACITY; i++) {
    buckets[i] = NULL;
  }

  /* Open addressing with linear probing over the seed-dependent home bucket. */
  for (size_t i = 0; i < KEY_COUNT; i++) {
    size_t slot = hash_key(KEYS[i], seed);
    while (buckets[slot] != NULL) {
      slot = (slot + 1) % CAPACITY;
    }
    buckets[slot] = KEYS[i];
  }

  /* Iterate in bucket order: the sequence depends on the seed. */
  int first = 1;
  for (size_t i = 0; i < CAPACITY; i++) {
    if (buckets[i] != NULL) {
      printf("%s%s", first ? "" : ",", buckets[i]);
      first = 0;
    }
  }
  putchar('\n');
  return 0;
}
