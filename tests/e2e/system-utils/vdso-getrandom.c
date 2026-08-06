/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reach entropy through the vDSO specifically, with no syscall of our own for a
 * tracer to intercept.
 *
 * This is a BACKEND-TARGETED case. `__vdso_getrandom` seeds a per-thread
 * userspace CSPRNG and then generates from it in user space, so once seeded
 * there is nothing on the syscall boundary to see. Under ptrace and DBI the
 * seeding is itself an intercepted `getrandom(2)`, so the output comes out
 * deterministic -- which makes the guarantee TRANSITIVE and fragile: it holds
 * only for a backend that intercepts that syscall. A guest exercising only
 * `getrandom(2)` reports success while this path is unconstrained, which is why
 * this case exists separately.
 *
 * The full vDSO ABI is used rather than a libc wrapper, because whether libc
 * routes `getrandom` through the vDSO is a libc-version detail that would make
 * the coverage silently optional.
 */

#define _GNU_SOURCE
#include <elf.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>

/* Filled by the query call; layout fixed by the kernel ABI. */
struct vgetrandom_opaque_params {
    uint32_t size_of_opaque_state;
    uint32_t mmap_prot;
    uint32_t mmap_flags;
    uint32_t reserved[13];
};

typedef long (*vdso_getrandom_fn)(void *, size_t, unsigned int, void *, size_t);

/*
 * Resolve a symbol in the kernel-supplied vDSO. Hand-parsed because the vDSO is
 * not in the loader's global namespace, so dlsym cannot see it.
 */
static void *vdso_symbol(const char *want) {
    unsigned char *base = (unsigned char *)getauxval(AT_SYSINFO_EHDR);
    if (!base) {
        return NULL;
    }
    Elf64_Ehdr *ehdr = (Elf64_Ehdr *)base;
    Elf64_Phdr *phdr = (Elf64_Phdr *)(base + ehdr->e_phoff);
    Elf64_Dyn *dynamic = NULL;
    long load_bias = 0;
    for (int i = 0; i < ehdr->e_phnum; i++) {
        if (phdr[i].p_type == PT_LOAD) {
            load_bias = (long)base - (long)(phdr[i].p_vaddr - phdr[i].p_offset);
        }
        if (phdr[i].p_type == PT_DYNAMIC) {
            dynamic = (Elf64_Dyn *)(base + phdr[i].p_offset);
        }
    }
    if (!dynamic) {
        return NULL;
    }
    const char *strtab = NULL;
    Elf64_Sym *symtab = NULL;
    long count = 0;
    for (Elf64_Dyn *d = dynamic; d->d_tag != DT_NULL; d++) {
        if (d->d_tag == DT_STRTAB) {
            strtab = (const char *)(d->d_un.d_ptr + load_bias);
        }
        if (d->d_tag == DT_SYMTAB) {
            symtab = (Elf64_Sym *)(d->d_un.d_ptr + load_bias);
        }
        /* DT_HASH's second word is the symbol count. */
        if (d->d_tag == DT_HASH) {
            count = ((Elf32_Word *)(d->d_un.d_ptr + load_bias))[1];
        }
    }
    if (!strtab || !symtab) {
        return NULL;
    }
    for (long i = 0; i < count; i++) {
        if (strcmp(strtab + symtab[i].st_name, want) == 0) {
            return (void *)(symtab[i].st_value + load_bias);
        }
    }
    return NULL;
}

int main(void) {
    vdso_getrandom_fn vdso_getrandom = (vdso_getrandom_fn)vdso_symbol("__vdso_getrandom");
    if (!vdso_getrandom) {
        /* Kernels before 6.11 do not export it. Report rather than skip, so an
         * absent symbol can never be mistaken for a determinized source. */
        printf("vdso_getrandom UNSUPPORTED_KERNEL\n");
        return 0;
    }

    /* Query mode: opaque_len == ~0 asks the vDSO to describe its state block. */
    struct vgetrandom_opaque_params params;
    memset(&params, 0, sizeof params);
    if (vdso_getrandom(NULL, 0, 0, &params, ~(size_t)0) != 0) {
        printf("vdso_getrandom QUERY_FAILED\n");
        return 1;
    }

    void *state = mmap(NULL, params.size_of_opaque_state, params.mmap_prot,
                       params.mmap_flags, -1, 0);
    if (state == MAP_FAILED) {
        printf("vdso_getrandom MMAP_FAILED\n");
        return 1;
    }

    unsigned char bytes[16];
    memset(bytes, 0, sizeof bytes);
    long n = vdso_getrandom(bytes, sizeof bytes, 0, state, params.size_of_opaque_state);
    if (n != (long)sizeof bytes) {
        /* A negative return means the vDSO declined and the caller is expected
         * to fall back to the syscall. Report it rather than falling back, so
         * this case never silently measures getrandom(2) instead. */
        printf("vdso_getrandom DECLINED %ld\n", n);
        return 1;
    }

    /* Emit the OBSERVED BYTES. A pass/fail count could not show two backends
     * agreeing on a wrong value; the value can. */
    printf("vdso_getrandom ");
    for (size_t i = 0; i < sizeof bytes; i++) {
        printf("%02x", bytes[i]);
    }
    printf("\n");
    return 0;
}
