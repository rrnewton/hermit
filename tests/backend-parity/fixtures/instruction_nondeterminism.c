/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <cpuid.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

enum {
  CPUID_SAMPLES = 8,
  RDTSC_SAMPLES = 16,
  RANDOM_SAMPLES = 8,
};

struct cpuid_result {
  uint32_t eax;
  uint32_t ebx;
  uint32_t ecx;
  uint32_t edx;
};

static struct cpuid_result read_cpuid(uint32_t leaf, uint32_t subleaf) {
  struct cpuid_result result;
  __cpuid_count(leaf, subleaf, result.eax, result.ebx, result.ecx, result.edx);
  return result;
}

static bool cpuid_equal(struct cpuid_result left, struct cpuid_result right) {
  return left.eax == right.eax && left.ebx == right.ebx &&
         left.ecx == right.ecx && left.edx == right.edx;
}

static uint64_t read_rdtsc(void) {
  uint32_t low;
  uint32_t high;
  __asm__ volatile("rdtsc" : "=a"(low), "=d"(high) : : "memory");
  return ((uint64_t)high << 32) | low;
}

static bool read_rdrand(uint64_t *value) {
  unsigned char available;
  __asm__ volatile("rdrand %0; setc %1"
                   : "=r"(*value), "=qm"(available)
                   :
                   : "cc");
  return available != 0;
}

static bool read_rdseed(uint64_t *value) {
  unsigned char available;
  __asm__ volatile("rdseed %0; setc %1"
                   : "=r"(*value), "=qm"(available)
                   :
                   : "cc");
  return available != 0;
}

static uint64_t mix(uint64_t state, uint64_t value) {
  state ^= value;
  state *= UINT64_C(1099511628211);
  return state;
}

static int check_cpuid(struct cpuid_result *feature,
                       struct cpuid_result *extended) {
  const struct cpuid_result identity = read_cpuid(0, 0);
  *feature = read_cpuid(1, 0);
  *extended = read_cpuid(7, 0);

  for (int sample = 1; sample < CPUID_SAMPLES; ++sample) {
    if (!cpuid_equal(identity, read_cpuid(0, 0)) ||
        !cpuid_equal(*feature, read_cpuid(1, 0)) ||
        !cpuid_equal(*extended, read_cpuid(7, 0))) {
      fprintf(stderr, "CPUID changed within one execution at sample=%d\n",
              sample);
      return 1;
    }
  }

  char vendor[13] = {0};
  memcpy(vendor, &identity.ebx, sizeof(identity.ebx));
  memcpy(vendor + 4, &identity.edx, sizeof(identity.edx));
  memcpy(vendor + 8, &identity.ecx, sizeof(identity.ecx));
  printf("cpuid samples=%d vendor=%s max=%08" PRIx32 " signature=%08" PRIx32
         " rdrand=%u rdseed=%u\n",
         CPUID_SAMPLES, vendor, identity.eax, feature->eax,
         (feature->ecx >> 30) & 1, (extended->ebx >> 18) & 1);
  return 0;
}

static int check_rdtsc(void) {
  uint64_t samples[RDTSC_SAMPLES];
  uint64_t trajectory = UINT64_C(1469598103934665603);

  for (int sample = 0; sample < RDTSC_SAMPLES; ++sample) {
    volatile uint64_t progress = 0;
    for (uint64_t branch = 0; branch < 128; ++branch) {
      if ((branch & 1) == 0) {
        progress += branch;
      }
    }
    (void)progress;
    samples[sample] = read_rdtsc();
    if (sample > 0) {
      if (samples[sample] <= samples[sample - 1]) {
        fprintf(stderr,
                "RDTSC did not advance continuously: sample=%d before=%" PRIu64
                " after=%" PRIu64 "\n",
                sample, samples[sample - 1], samples[sample]);
        return 2;
      }
      trajectory = mix(trajectory, samples[sample] - samples[sample - 1]);
    }
  }

  printf("rdtsc samples=%d first=%" PRIu64 " last=%" PRIu64
         " trajectory=%016" PRIx64 "\n",
         RDTSC_SAMPLES, samples[0], samples[RDTSC_SAMPLES - 1], trajectory);
  return 0;
}

static void exercise_random(const char *name, bool (*instruction)(uint64_t *)) {
  unsigned successes = 0;
  uint64_t digest = UINT64_C(1469598103934665603);

  for (int sample = 0; sample < RANDOM_SAMPLES; ++sample) {
    uint64_t value = 0;
    const bool available = instruction(&value);
    successes += available;
    digest = mix(digest, value);
    digest = mix(digest, available);
  }

  printf("%s attempts=%d successes=%u digest=%016" PRIx64 "\n", name,
         RANDOM_SAMPLES, successes, digest);
}

int main(int argc, char **argv) {
  bool force_random = false;
  if (argc == 2 && strcmp(argv[1], "--respect-features") == 0) {
    force_random = false;
  } else if (argc == 2 && (strcmp(argv[1], "--force-random") == 0 ||
                           strcmp(argv[1], "--plant-nondeterminism") == 0)) {
    force_random = true;
  } else if (argc != 1) {
    fprintf(stderr,
            "usage: %s "
            "[--respect-features|--force-random|--plant-nondeterminism]\n",
            argv[0]);
    return 64;
  }

  struct cpuid_result feature;
  struct cpuid_result extended;
  int result = check_cpuid(&feature, &extended);
  if (result != 0) {
    return result;
  }
  result = check_rdtsc();
  if (result != 0) {
    return result;
  }

  const bool has_rdrand = ((feature.ecx >> 30) & 1) != 0;
  const bool has_rdseed = ((extended.ebx >> 18) & 1) != 0;
  if (force_random || has_rdrand) {
    exercise_random("rdrand", read_rdrand);
  } else {
    puts("rdrand masked-by-cpuid");
  }
  if (force_random || has_rdseed) {
    exercise_random("rdseed", read_rdseed);
  } else {
    puts("rdseed masked-by-cpuid");
  }

  puts("instruction nondeterminism probe success");
  return 0;
}
