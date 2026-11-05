/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Freestanding x86-64 ELF loader. No libc, heap allocation or initial-stack
 * writes except the explicitly patched auxv fields and AT_EXECFN string.
 * Contract: fd 100 pins the target; fd 102 contains execfn\0comm\0flags.
 * The host prepares the shadow PT_LOAD and disables address randomization.
 * This crate is deliberately not connected to Reverie or Hermit. */
#include <elf.h>
#include <linux/magic.h>
#include <linux/mman.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#ifdef LOADER_STACK_GUARD_CONTROL
#include <stdio.h>
#endif

#define PAGE 4096UL
#define PATH_LIMIT 4096UL
#define PH_LIMIT 128
#define USER_LIMIT 0x800000000000UL
#define STACK_TOP (USER_LIMIT - PAGE)
#define MAP_LIMIT STACK_TOP
#define INITIAL_STACK_LOW (STACK_TOP - 8UL * 1024 * 1024)
#define PSTART(x) ((x) & ~(PAGE - 1))
#define PALIGN(x) (((x) + PAGE - 1) & ~(PAGE - 1))
#define POFF(x) ((x) & (PAGE - 1))
#define ELF_ET_DYN_BASE 0x555555554aaaUL
#define TARGET_FD 100
#define METADATA_FD 102
#define AT_FDCWD_VALUE (-100)
#define O_RDONLY_VALUE 0
#define O_CLOEXEC_VALUE 02000000
#define O_PATH_VALUE 010000000
#define ADDR_NO_RANDOMIZE_VALUE 0x40000
#define PROT_R 1
#define PROT_W 2
#define PROT_X 4
#define MAP_PRIV 2
#define MAP_FIX 0x10
#define MAP_ANON_VALUE 0x20
#define MAP_NOREPLACE 0x100000
#define PR_SET_NAME_VALUE 15
#define PR_GET_MDWE_VALUE 66
#define PR_MDWE_REFUSE_EXEC_GAIN_VALUE 1UL
#define MIN_PROGRAM_ADDRESS 0x400000UL
#define MAX_MAPPING_SIZE (16UL * 1024 * 1024 * 1024)
/* For 256 load intervals plus two first reservations of at most 16 GiB,
 * a gap larger than one complete reservation plus x86's maximum 1 GiB file
 * alignment and the special-map span remains above our low band, even at
 * the lowest native mmap base. Boot stack-guard overrides are refused so
 * stack table growth cannot change this allocator's search ceiling. */
#define MIN_MMAP_BASE PALIGN(MAP_LIMIT - (MAP_LIMIT / 6) * 5)
_Static_assert(MIN_MMAP_BASE - MIN_PROGRAM_ADDRESS >
               (2UL * PH_LIMIT + 2) * MAX_MAPPING_SIZE +
               (2UL * PH_LIMIT + 3) * (MAX_MAPPING_SIZE + (1UL << 30) + 0x100000),
               "allocator free-gap bound");
#define TEMP_SPECIAL_START 0x20000UL
#define LOADER_CODE_START 0x100000UL
#define MAX_SPECIAL_SIZE 0x80000UL

struct handoff_record {
    unsigned long sp;
    unsigned long entry;
    unsigned long mutations;
};
_Static_assert(sizeof(struct handoff_record) == 24, "assembly record layout");
struct handoff_record loader_record __attribute__((section(".record")));
unsigned char loader_stack[65536] __attribute__((aligned(16)));
unsigned char loader_xstate[65536] __attribute__((aligned(64)));
const uint32_t loader_mxcsr = 0x1f80;

static Elf64_Ehdr target_header, interp_header;
static Elf64_Phdr target_phdrs[PH_LIMIT], interp_phdrs[PH_LIMIT];
static char interp_name[PATH_LIMIT];
static char metadata[PATH_LIMIT + 17];
static char maps_buffer[65536];
static int mdwe_refuse_exec_gain;
/* Each file/BSS mapping introduces at most two surviving boundaries. Both
 * images have at most PH_LIMIT loads, hence four times PH_LIMIT intervals
 * suffice. These records live in shared scratch, not private data_vm. */
#define BSS_LIMIT (4 * PH_LIMIT)
struct bss_range { unsigned long low, high; int executable; };
static struct bss_range bss_ranges[BSS_LIMIT];
static unsigned int bss_count;

extern void loader_enter(void) __attribute__((noreturn));

static long sc6(long nr, long a, long b, long c, long d, long e, long f) {
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"(nr), "D"(a), "S"(b), "d"(c),
                       "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory", "cc");
    return result;
}
#define sc(nr, a, b, c) sc6(nr, (long)(a), (long)(b), (long)(c), 0, 0, 0)

void *memset(void *dst, int value, size_t size) {
    unsigned char *p = dst;
    while (size--) *p++ = (unsigned char)value;
    return dst;
}
void *memcpy(void *dst, const void *src, size_t size) {
    unsigned char *p = dst;
    const unsigned char *q = src;
    while (size--) *p++ = *q++;
    return dst;
}
static size_t bounded_length(const char *s, size_t limit) {
    size_t n = 0;
    while (n < limit && s[n]) n++;
    return n;
}
static int same_bytes(const char *a, const char *b, size_t n) {
    for (size_t i = 0; i < n; i++) if (a[i] != b[i]) return 0;
    return 1;
}
static int is_error(long value) {
    return (unsigned long)value >= (unsigned long)-4095;
}
static void die(const char *message, long value) __attribute__((noreturn));
static void die(const char *message, long value) {
    char buffer[192], digits[20];
    const char *prefix = "reverie-elf-loader: ";
    size_t n = 0, k = 0;
    while (*prefix) buffer[n++] = *prefix++;
    while (*message && n < 150) buffer[n++] = *message++;
    buffer[n++] = ' ';
    unsigned long u = (unsigned long)value;
    if (value < 0) { buffer[n++] = '-'; u = 0UL - u; }
    do { digits[k++] = (char)('0' + u % 10); u /= 10; } while (u);
    while (k) buffer[n++] = digits[--k];
    buffer[n++] = '\n';
    size_t done = 0;
    while (done < n) {
        long written = sc(SYS_write, 2, buffer + done, n - done);
        if (written == -4) continue;
        if (written <= 0) break;
        done += (size_t)written;
    }
    sc(SYS_exit_group, 127, 0, 0);
    __builtin_unreachable();
}
static void checked_close(int fd) {
    long result = sc(SYS_close, fd, 0, 0);
    if (is_error(result)) die("close descriptor", result);
}
static void check_stack_guard_cmdline(const char *cmdline, size_t size) {
    const char key[] = "stack_guard_gap=";
    if (size < sizeof key - 1) return;
    for (size_t i = 0; i <= size - (sizeof key - 1); i++)
        if (same_bytes(cmdline + i, key, sizeof key - 1))
            die("stack_guard_gap boot override unsupported", -22);
}
static void check_stack_guard_host(void) {
    long fd = sc(SYS_openat, AT_FDCWD_VALUE, "/proc/cmdline",
                 O_RDONLY_VALUE | O_CLOEXEC_VALUE);
    if (is_error(fd)) die("read boot stack guard", fd);
    size_t n = 0;
    for (;;) {
        if (n == sizeof maps_buffer) die("boot cmdline exceeds loader capacity", n);
        long count = sc(SYS_read, fd, maps_buffer + n, sizeof maps_buffer - n);
        if (count == -4) continue;
        if (is_error(count)) die("read boot stack guard", count);
        if (!count) break;
        n += (size_t)count;
    }
    checked_close((int)fd);
    check_stack_guard_cmdline(maps_buffer, n);
}
static int ascii_space(unsigned char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\f' || c == '\r';
}
static unsigned long parse_mmap_min_addr(const char *text, size_t size) {
    size_t i = 0;
    while (i < size && ascii_space((unsigned char)text[i])) i++;
    unsigned long value = 0;
    size_t digits = 0;
    while (i < size && text[i] >= '0' && text[i] <= '9') {
        unsigned long digit = (unsigned long)(text[i++] - '0');
        if (value > (~0UL - digit) / 10) die("invalid vm.mmap_min_addr", -22);
        value = value * 10 + digit;
        digits++;
    }
    while (i < size && ascii_space((unsigned char)text[i])) i++;
    if (!digits || i != size) die("invalid vm.mmap_min_addr", -22);
    return value;
}
static unsigned long temporary_special_start(unsigned long minimum) {
    /* Reserve enough room for every supported special-map geometry. Check
     * before rounding so even an overflowing sysctl value is refused by name. */
    if (minimum > LOADER_CODE_START - MAX_SPECIAL_SIZE)
        die("vm.mmap_min_addr exceeds temporary special mapping band", -22);
    unsigned long start = PALIGN(minimum);
    return start < TEMP_SPECIAL_START ? TEMP_SPECIAL_START : start;
}
static unsigned long temporary_special_host_start(void) {
    long fd = sc(SYS_openat, AT_FDCWD_VALUE, "/proc/sys/vm/mmap_min_addr",
                 O_RDONLY_VALUE | O_CLOEXEC_VALUE);
    if (is_error(fd)) die("read vm.mmap_min_addr", fd);
    char buffer[64];
    size_t n = 0;
    for (;;) {
        if (n == sizeof buffer) die("invalid vm.mmap_min_addr", -22);
        long count = sc(SYS_read, fd, buffer + n, sizeof buffer - n);
        if (count == -4) continue;
        if (is_error(count)) die("read vm.mmap_min_addr", count);
        if (!count) break;
        n += (size_t)count;
    }
    checked_close((int)fd);
    return temporary_special_start(parse_mmap_min_addr(buffer, n));
}
static void pread_exact(int fd, void *dst, unsigned long n, unsigned long off) {
    unsigned char *p = dst;
    unsigned long done = 0;
    while (done < n) {
        long result = sc6(SYS_pread64, fd, (long)(p + done), n - done,
                          off + done, 0, 0);
        if (result == -4) continue;
        if (result <= 0) die("read ELF or metadata", result);
        done += (unsigned long)result;
    }
}
static void checked_unmap(unsigned long start, unsigned long length) {
    if (!length) return;
    long result = sc(SYS_munmap, start, length, 0);
    if (is_error(result)) die("unmap", result);
}
static int prot_of(uint32_t flags) {
    return ((flags & PF_R) ? PROT_R : 0) |
           ((flags & PF_W) ? PROT_W : 0) |
           ((flags & PF_X) ? PROT_X : 0);
}

static void check_mapping_size(unsigned long length) {
    if (length > MAX_MAPPING_SIZE)
        die("mapping exceeds allocator size bound", -22);
}

static void check_mapping_range(unsigned long start, unsigned long length,
                                int interpreter) {
    if (!length) return;
    if (start < MIN_PROGRAM_ADDRESS)
        die(interpreter ? "interpreter PT_LOAD overlaps loader reserved range" :
                          "program PT_LOAD overlaps loader reserved range", -22);
    if (start >= USER_LIMIT || length > USER_LIMIT - start)
        die("biased PT_LOAD exceeds user address range", -22);
    if (start >= MAP_LIMIT || length > MAP_LIMIT - start)
        die("PT_LOAD reaches reserved top page", -22);
    check_mapping_size(length);
    if (start < STACK_TOP && start + length > INITIAL_STACK_LOW)
        die("PT_LOAD overlaps initial stack", -22);
}
static void check_load_range(unsigned long page, const Elf64_Phdr *p,
                             int interpreter) {
    /* Use the original ELF page offset, including when no_base made the
     * first request zero. An entirely empty load has no mapping to check. */
    if (page >= USER_LIMIT || POFF(p->p_vaddr) >= USER_LIMIT - page ||
        p->p_memsz > USER_LIMIT - page - POFF(p->p_vaddr))
        die("biased PT_LOAD exceeds user address range", -22);
    if (page >= MAP_LIMIT || POFF(p->p_vaddr) >= MAP_LIMIT - page ||
        p->p_memsz > MAP_LIMIT - page - POFF(p->p_vaddr))
        die("PT_LOAD reaches reserved top page", -22);
    if (p->p_memsz)
        check_mapping_range(page, PALIGN(POFF(p->p_vaddr) + p->p_memsz), interpreter);
}

static void read_elf(int fd, Elf64_Ehdr *header, Elf64_Phdr *phdrs) {
    struct stat statbuf;
    long result = sc(SYS_fstat, fd, &statbuf, 0);
    if (is_error(result)) die("stat ELF", result);
    if (!S_ISREG(statbuf.st_mode)) die("ELF is not a regular file", 0);
    struct statfs filesystem;
    result = sc(SYS_fstatfs, fd, &filesystem, 0);
    if (is_error(result)) die("stat ELF filesystem", result);
    if (filesystem.f_type == HUGETLBFS_MAGIC)
        die("hugetlb ELF file mapping semantics unsupported", -22);
    if (fd == TARGET_FD) {
        if (statbuf.st_mode & (S_ISUID | S_ISGID))
            die("privileged executable would change exec credentials", -22);
        result = sc6(SYS_fgetxattr, fd, (long)"security.capability", 0, 0, 0, 0);
        if (result >= 0)
            die("privileged executable would change exec credentials", -22);
        if (result != -61 && result != -95) /* ENODATA or EOPNOTSUPP */
            die("query executable security.capability", result);
    }
    pread_exact(fd, header, sizeof *header, 0);
    if (!same_bytes((const char *)header->e_ident, ELFMAG, SELFMAG) ||
        header->e_ident[EI_CLASS] != ELFCLASS64 ||
        header->e_ident[EI_DATA] != ELFDATA2LSB ||
        header->e_ident[EI_VERSION] != EV_CURRENT ||
        header->e_machine != EM_X86_64 || header->e_version != EV_CURRENT ||
        (header->e_type != ET_EXEC && header->e_type != ET_DYN) ||
        header->e_ehsize != sizeof *header ||
        header->e_phentsize != sizeof *phdrs ||
        !header->e_phnum || header->e_phnum > PH_LIMIT)
        die("unsupported ELF header", header->e_phnum);
    unsigned long phsize = header->e_phnum * sizeof *phdrs;
    if (statbuf.st_size < 0 || header->e_phoff > (unsigned long)statbuf.st_size ||
        phsize > (unsigned long)statbuf.st_size - header->e_phoff)
        die("program headers outside file", 0);
    pread_exact(fd, phdrs, phsize, header->e_phoff);
    int load_count = 0;
    unsigned long last_vaddr = 0;
    for (int i = 0; i < header->e_phnum; i++) {
        const Elf64_Phdr *p = &phdrs[i];
        if (p->p_type == PT_LOAD) {
            if (load_count && p->p_vaddr < last_vaddr)
                die("unordered ELF load segments", i);
            last_vaddr = p->p_vaddr;
            load_count++;
            if (p->p_filesz > p->p_memsz || p->p_vaddr >= USER_LIMIT ||
                p->p_memsz > USER_LIMIT - p->p_vaddr ||
                p->p_offset > (unsigned long)statbuf.st_size ||
                p->p_filesz > (unsigned long)statbuf.st_size - p->p_offset ||
                POFF(p->p_offset) != POFF(p->p_vaddr) ||
                (p->p_align > 1 && (p->p_align & (p->p_align - 1))))
                die("unsupported ELF load geometry", i);
            if (p->p_vaddr >= MAP_LIMIT || p->p_memsz > MAP_LIMIT - p->p_vaddr)
                die("PT_LOAD reaches reserved top page", -22);
        }
    }
    if (!load_count) die("ELF has no load segments", 0);
}
static unsigned long total_mapping_size(const Elf64_Phdr *phdrs, int n) {
    unsigned long low = ~0UL, high = 0;
    for (int i = 0; i < n; i++) if (phdrs[i].p_type == PT_LOAD) {
        if (PSTART(phdrs[i].p_vaddr) < low) low = PSTART(phdrs[i].p_vaddr);
        unsigned long end = phdrs[i].p_vaddr + phdrs[i].p_memsz;
        if (end > high) high = end;
    }
    return high - low;
}
static unsigned long maximum_alignment(const Elf64_Phdr *phdrs, int n) {
    unsigned long align = 0;
    for (int i = 0; i < n; i++) if (phdrs[i].p_type == PT_LOAD) {
        unsigned long a = phdrs[i].p_align;
        if (a && !(a & (a - 1)) && a > align) align = a;
    }
    return PALIGN(align);
}

static void forget_bss(unsigned long start, unsigned long length) {
    unsigned long end = start + length;
    for (unsigned int i = 0; i < bss_count;) {
        struct bss_range old = bss_ranges[i];
        if (old.low >= end || old.high <= start) { i++; continue; }
        if (old.low < start && old.high > end) {
            if (bss_count == BSS_LIMIT) die("BSS tracking exceeds capacity", bss_count);
            bss_ranges[bss_count++] = (struct bss_range){end, old.high, old.executable};
            bss_ranges[i].high = start;
            i++;
        } else if (old.low < start) {
            bss_ranges[i].high = start;
            i++;
        } else if (old.high > end) {
            bss_ranges[i].low = end;
            i++;
        } else {
            bss_ranges[i] = bss_ranges[--bss_count];
        }
    }
}
static void check_bss_right(unsigned long end, int executable) {
    for (unsigned int i = 0; i < bss_count; i++)
        if (bss_ranges[i].low <= end && bss_ranges[i].high > end &&
            bss_ranges[i].executable == executable)
            die("anonymous BSS would merge with right VMA", -22);
}
static void remember_bss(unsigned long start, unsigned long length, int executable) {
    forget_bss(start, length);
    if (bss_count == BSS_LIMIT) die("BSS tracking exceeds capacity", bss_count);
    bss_ranges[bss_count++] = (struct bss_range){start, start + length, executable};
}

/* Linux binfmt_elf's elf_map reserves the entire load span on the first
 * mapping, then releases the unused tail before mapping later segments. */
static long elf_map(int fd, unsigned long addr, const Elf64_Phdr *p,
                    int prot, int flags, unsigned long total, int interpreter) {
    unsigned long size = PALIGN(p->p_filesz + POFF(p->p_vaddr));
    unsigned long offset = p->p_offset - POFF(p->p_vaddr);
    addr = PSTART(addr);
    if (!size) return (long)addr;
    unsigned long length = total ? PALIGN(total) : size;
    if (length < size) die("invalid total ELF mapping length", 0);
    check_mapping_size(length);
    if (flags & (MAP_FIX | MAP_NOREPLACE))
        check_mapping_range(addr, length, interpreter);
    long mapped = sc6(SYS_mmap, addr, length, prot, flags, fd, offset);
    if (!is_error(mapped)) {
        /* A nonfixed zero hint must reach mmap. Validate its actual entire
         * reservation before releasing the tail or touching BSS. */
        check_mapping_range((unsigned long)mapped, length, interpreter);
        // A fixed first reservation removes prior BSS even in its released
        // tail. Track the complete mmap extent, rather than only file pages.
        forget_bss((unsigned long)mapped, length);
        if (total && length > size)
            checked_unmap((unsigned long)mapped + size, length - size);
    }
    return mapped;
}
static long elf_load(int fd, unsigned long addr, const Elf64_Phdr *p,
                     int prot, int flags, unsigned long total, int interpreter) {
    unsigned long zero_start, zero_end;
    long mapped;
    if (!p->p_filesz || (flags & (MAP_FIX | MAP_NOREPLACE)))
        check_load_range(PSTART(addr), p, interpreter);
    if (p->p_filesz) {
        mapped = elf_map(fd, addr, p, prot, flags, total, interpreter);
        if (is_error(mapped)) return mapped;
        check_load_range((unsigned long)mapped, p, interpreter);
        zero_start = (unsigned long)mapped + POFF(p->p_vaddr) + p->p_filesz;
        zero_end = (unsigned long)mapped + POFF(p->p_vaddr) + p->p_memsz;
        if (p->p_memsz > p->p_filesz && (prot & PROT_W) && POFF(zero_start))
            memset((void *)zero_start, 0, PAGE - POFF(zero_start));
    } else {
        mapped = (long)PSTART(addr);
        zero_start = (unsigned long)mapped;
        zero_end = zero_start + POFF(p->p_vaddr) + p->p_memsz;
    }
    if (p->p_memsz > p->p_filesz) {
        zero_start = PALIGN(zero_start);
        zero_end = PALIGN(zero_end);
        if (zero_end > zero_start) {
            /* binfmt_elf's vm_brk_flags can create executable BSS under
             * MDWE, but user mmap cannot. Preparation refuses this scope;
             * repeat the check for MDWE enabled after preparing the image. */
            if ((prot & PROT_X) && mdwe_refuse_exec_gain)
                die("MDWE PR_MDWE_REFUSE_EXEC_GAIN refuses executable BSS", -22);
            check_mapping_range(zero_start, zero_end - zero_start, interpreter);
            /* vm_brk_flags only merges with the previous VMA; anonymous mmap
             * can also merge right. Refuse that geometry before installing it. */
            check_bss_right(zero_end, !!(prot & PROT_X));
            long result = sc6(SYS_mmap, zero_start, zero_end - zero_start,
                              PROT_R | PROT_W | (prot & PROT_X),
                              MAP_PRIV | MAP_ANON_VALUE | MAP_FIX, -1, 0);
            if (is_error(result)) return result;
            remember_bss(zero_start, zero_end - zero_start, !!(prot & PROT_X));
        }
    }
    return mapped;
}

struct vm { unsigned long low, high; };
static unsigned long parse_hex(const char **p, const char *end) {
    unsigned long value = 0;
    int digits = 0;
    while (*p < end) {
        char c = **p;
        unsigned long d;
        if (c >= '0' && c <= '9') d = (unsigned long)(c - '0');
        else if (c >= 'a' && c <= 'f') d = (unsigned long)(c - 'a' + 10);
        else break;
        if (value > (~0UL >> 4)) die("overflow parsing maps", 0);
        value = value * 16 + d;
        (*p)++;
        digits++;
    }
    if (!digits) die("invalid maps address", 0);
    return value;
}
static int find_special(struct vm out[3], unsigned long temporary_start) {
    long fd = sc(SYS_openat, AT_FDCWD_VALUE, "/proc/self/maps",
                 O_RDONLY_VALUE | O_CLOEXEC_VALUE);
    if (is_error(fd)) die("open maps", fd);
    size_t n = 0;
    for (;;) {
        if (n == sizeof maps_buffer - 1) die("maps exceeds loader capacity", n);
        long count = sc(SYS_read, fd, maps_buffer + n, sizeof maps_buffer - 1 - n);
        if (count == -4) continue;
        if (is_error(count)) die("read maps", count);
        if (!count) break;
        n += (size_t)count;
    }
    checked_close((int)fd);
    maps_buffer[n] = 0;
    const char *names[3] = {"[vvar]", "[vvar_vclock]", "[vdso]"};
    int found = 0;
    const char *end = maps_buffer + n;
    for (const char *line = maps_buffer; line < end;) {
        const char *p = line;
        unsigned long low = parse_hex(&p, end);
        if (p == end || *p++ != '-') die("invalid maps separator", 0);
        unsigned long high = parse_hex(&p, end);
        if (high <= low || POFF(low) || POFF(high)) die("invalid maps range", 0);
        if (low < temporary_start + MAX_SPECIAL_SIZE && high > temporary_start)
            die("temporary special mapping band is not free", -22);
        const char *next = p;
        while (next < end && *next != '\n') next++;
        const char *name = next;
        while (name > line && name[-1] != ' ') name--;
        for (int i = 0; i < 3; i++) {
            size_t length = bounded_length(names[i], 20);
            if ((size_t)(next - name) == length && same_bytes(name, names[i], length)) {
                if (found & (1 << i)) die("duplicate special mapping", i);
                out[i].low = low; out[i].high = high;
                found |= 1 << i;
            }
        }
        line = next < end ? next + 1 : end;
    }
    if ((found & 5) != 5) die("required vvar or vdso mapping absent", found);
    return found;
}
static void move_special(const struct vm ranges[3], int found,
                         unsigned long old_low, unsigned long new_low,
                         int temporary) {
    if (new_low >= USER_LIMIT) die("special mapping destination exceeds user range", -22);
    for (int i = 0; i < 3; i++) if (found & (1 << i)) {
        unsigned long size = ranges[i].high - ranges[i].low;
        if (ranges[i].low < old_low || ranges[i].low - old_low > USER_LIMIT - new_low)
            die("special mapping destination overflows", -22);
        unsigned long to = new_low + (ranges[i].low - old_low);
        if (temporary) {
            if (to < TEMP_SPECIAL_START || to >= LOADER_CODE_START ||
                size > LOADER_CODE_START - to)
                die("temporary special mapping overlaps loader", -22);
        } else if (to < MIN_PROGRAM_ADDRESS || to >= USER_LIMIT || size > USER_LIMIT - to) {
            die("native special mapping overlaps loader reserved range", -22);
        }
        long result = sc6(SYS_mremap, ranges[i].low, size, size,
                          MREMAP_MAYMOVE | MREMAP_FIXED, to, 0);
        if (is_error(result) || (unsigned long)result != to)
            die("relocate vvar or vdso", result);
    }
}

static unsigned long read_metadata(char **comm) {
    struct stat statbuf;
    long result = sc(SYS_fstat, METADATA_FD, &statbuf, 0);
    if (is_error(result)) die("stat metadata", result);
    if (statbuf.st_size < 3 || (unsigned long)statbuf.st_size > sizeof metadata)
        die("invalid metadata length", statbuf.st_size);
    size_t size = (size_t)statbuf.st_size;
    pread_exact(METADATA_FD, metadata, size, 0);
    size_t exec_len = bounded_length(metadata, size);
    if (!exec_len || exec_len >= PATH_LIMIT || exec_len + 2 >= size)
        die("invalid native execfn length", exec_len);
    *comm = metadata + exec_len + 1;
    size_t comm_len = bounded_length(*comm, size - exec_len - 1);
    if (comm_len >= 16 || exec_len + 1 + comm_len + 2 != size)
        die("invalid native comm length", comm_len);
    unsigned long flags = (unsigned char)metadata[size - 1];
#ifdef LOADER_TEST_MUTATIONS
    if (flags & ~127UL) die("unknown test mutation", flags);
#else
    if (flags) die("test mutations unavailable in production loader", flags);
#endif
    loader_record.mutations = flags;
    checked_close(METADATA_FD);
    return exec_len;
}
static int open_interpreter(void) {
    long pathfd = sc(SYS_openat, AT_FDCWD_VALUE, interp_name,
                     O_PATH_VALUE | O_CLOEXEC_VALUE);
    if (is_error(pathfd)) die("pin interpreter", pathfd);
    struct stat statbuf;
    long result = sc(SYS_fstat, pathfd, &statbuf, 0);
    if (is_error(result)) die("stat interpreter", result);
    if (!S_ISREG(statbuf.st_mode)) die("interpreter is not a regular file", 0);
    /* Reopen the pinned object, rather than repeat the pathname lookup. O_PATH
     * classification avoids opening a FIFO or executing a device open method. */
    char path[40];
    const char *prefix = "/proc/self/fd/";
    size_t n = 0;
    while (*prefix) path[n++] = *prefix++;
    char digits[20]; size_t count = 0;
    unsigned long fdnumber = (unsigned long)pathfd;
    do { digits[count++] = (char)('0' + fdnumber % 10); fdnumber /= 10; } while (fdnumber);
    while (count) path[n++] = digits[--count];
    path[n] = 0;
    long fd = sc(SYS_openat, AT_FDCWD_VALUE, path, O_RDONLY_VALUE | O_CLOEXEC_VALUE);
    if (is_error(fd)) die("read access to pinned interpreter denied", fd);
    checked_close((int)pathfd);
    return (int)fd;
}
static void validate_extended_state(void) {
    unsigned int a, b, c, d;
    __asm__ volatile("cpuid" : "=a"(a), "=b"(b), "=c"(c), "=d"(d)
                     : "a"(1U), "c"(0U));
    if ((c & 0x0c000000U) != 0x0c000000U) return;
    __asm__ volatile("cpuid" : "=a"(a), "=b"(b), "=c"(c), "=d"(d)
                     : "a"(13U), "c"(0U));
    if (b > sizeof loader_xstate || c > sizeof loader_xstate)
        die("extended state exceeds loader capacity", c);
}

void loader_main(unsigned long *sp) __attribute__((noreturn));
void loader_main(unsigned long *sp) {
    long personality = sc(SYS_personality, 0xffffffffUL, 0, 0);
    if (is_error(personality) || !(personality & ADDR_NO_RANDOMIZE_VALUE))
        die("address randomization must be disabled", personality);
    check_stack_guard_host();
    validate_extended_state();
    long mdwe = sc(SYS_prctl, PR_GET_MDWE_VALUE, 0, 0);
    if (is_error(mdwe) && mdwe != -22) die("query MDWE PR_GET_MDWE", mdwe);
    mdwe_refuse_exec_gain = mdwe >= 0 && (mdwe & PR_MDWE_REFUSE_EXEC_GAIN_VALUE);
    unsigned long argc = sp[0];
    if (argc > 0x100000) die("invalid initial argc", argc);
    char **argv = (char **)(sp + 1);
    if (argv[argc]) die("invalid argv terminator", 0);
    char **envp = argv + argc + 1;
    char **e = envp;
    while (*e) e++;
    Elf64_auxv_t *auxv = (Elf64_auxv_t *)(e + 1);
    unsigned long self_phdr_addr = 0, self_phnum = 0;
    char *execfn = 0;
    int aux_count = 0;
    for (Elf64_auxv_t *x = auxv; x->a_type != AT_NULL; x++) {
        if (++aux_count > 128) die("auxv exceeds loader capacity", aux_count);
        if (x->a_type == AT_PHDR) self_phdr_addr = x->a_un.a_val;
        if (x->a_type == AT_PHNUM) self_phnum = x->a_un.a_val;
        if (x->a_type == AT_EXECFN) execfn = (char *)x->a_un.a_val;
    }
    char *comm;
    unsigned long exec_length = read_metadata(&comm);
    if (!execfn || bounded_length(execfn, PATH_LIMIT) != exec_length)
        die("AT_EXECFN padded length mismatch", exec_length);
    read_elf(TARGET_FD, &target_header, target_phdrs);
    int interp_count = 0, stack_count = 0;
    for (int i = 0; i < target_header.e_phnum; i++) {
        const Elf64_Phdr *p = &target_phdrs[i];
        if (p->p_type == PT_INTERP) {
            if (++interp_count != 1 || p->p_filesz < 2 || p->p_filesz > sizeof interp_name)
                die("unsupported interpreter name", p->p_filesz);
            pread_exact(TARGET_FD, interp_name, p->p_filesz, p->p_offset);
            if (interp_name[p->p_filesz - 1] ||
                bounded_length(interp_name, p->p_filesz) != p->p_filesz - 1)
                die("invalid interpreter string", 0);
        }
        if (p->p_type == PT_GNU_STACK) {
            stack_count++;
            if (p->p_flags & PF_X) die("executable initial stack unsupported", 0);
        }
    }
    if (interp_count != 1) die("target must have PT_INTERP", interp_count);
    if (stack_count != 1) die("target must have non-executable PT_GNU_STACK", stack_count);
    int interp_fd = open_interpreter();
    read_elf(interp_fd, &interp_header, interp_phdrs);

    unsigned long target_bias = 0, first_vaddr = 0;
    for (int i = 0; i < target_header.e_phnum; i++) if (target_phdrs[i].p_type == PT_LOAD) {
        first_vaddr = target_phdrs[i].p_vaddr;
        break;
    }
    if (target_header.e_type == ET_DYN) {
        unsigned long alignment = maximum_alignment(target_phdrs, target_header.e_phnum);
        unsigned long base = ELF_ET_DYN_BASE;
        if (alignment) base &= ~(alignment - 1);
        if (first_vaddr > base) die("PIE first segment exceeds native load base", -22);
        target_bias = PSTART(base - first_vaddr);
    }
    unsigned long start_data = 0, end_data = 0, elf_brk = 0;
    for (int i = 0; i < target_header.e_phnum; i++) if (target_phdrs[i].p_type == PT_LOAD) {
        const Elf64_Phdr *p = &target_phdrs[i];
        unsigned long address = target_bias + p->p_vaddr;
        check_load_range(PSTART(address), p, 0);
        if (PSTART(address) < MIN_PROGRAM_ADDRESS)
            die("program PT_LOAD overlaps loader reserved range", -22);
        if (address > start_data) start_data = address;
        if (address + p->p_filesz > end_data) end_data = address + p->p_filesz;
        if (address + p->p_memsz > elf_brk) elf_brk = address + p->p_memsz;
    }
    unsigned long native_brk = PALIGN(elf_brk);

    /* With no first file mmap, load_elf_interp establishes a known bias
     * without reserving the span. Preflight EVERY effective load, including
     * empty headers, before a later fixed file/BSS mapping can hit us. */
    const Elf64_Phdr *interp_first = 0;
    for (int i = 0; i < interp_header.e_phnum; i++) if (interp_phdrs[i].p_type == PT_LOAD) {
        interp_first = &interp_phdrs[i];
        break;
    }
    if (interp_header.e_type == ET_EXEC || !interp_first->p_filesz) {
        unsigned long origin = interp_header.e_type == ET_DYN && target_bias ?
                               PSTART(interp_first->p_vaddr) : 0;
        for (int i = 0; i < interp_header.e_phnum; i++) if (interp_phdrs[i].p_type == PT_LOAD) {
            const Elf64_Phdr *p = &interp_phdrs[i];
            if (p->p_vaddr < origin) die("interpreter load precedes its first page", -22);
            check_load_range(PSTART(p->p_vaddr - origin), p, 1);
        }
        unsigned long entry = interp_header.e_entry - origin;
        if (entry < MIN_PROGRAM_ADDRESS || entry >= MAP_LIMIT)
            die("interpreter entry outside admitted address range", -22);
    } else if (interp_header.e_entry >= MAP_LIMIT) {
        die("interpreter entry outside admitted address range", -22);
    }

    struct vm special[3] = {{0, 0}, {0, 0}, {0, 0}};
    unsigned long temporary_start = temporary_special_host_start();
    int found = find_special(special, temporary_start);
    unsigned long special_low = ~0UL, special_high = 0;
    for (int i = 0; i < 3; i++) if (found & (1 << i)) {
        if (special[i].low < special_low) special_low = special[i].low;
        if (special[i].high > special_high) special_high = special[i].high;
    }
    unsigned long special_size = special_high - special_low;
    if (special_size > MAX_SPECIAL_SIZE) die("special mappings exceed scratch band", special_size);
    int skip_vdso = 0, missing_shadow = 0;
#ifdef LOADER_TEST_MUTATIONS
    skip_vdso = !!(loader_record.mutations & 1);
    missing_shadow = !!(loader_record.mutations & 4);
#endif
    if (!skip_vdso) move_special(special, found, special_low, temporary_start, 1);

    /* Drop exactly the placeholder installed in this loader image, rather
     * than blindly unmapping addresses inferred from an untrusted target. */
    if (!self_phdr_addr || self_phnum > PH_LIMIT) die("invalid loader phdrs", self_phnum);
    const Elf64_Phdr *self_phdrs = (const Elf64_Phdr *)self_phdr_addr;
    const Elf64_Phdr *shadow = 0;
    for (unsigned long i = 0; i < self_phnum; i++) {
        const Elf64_Phdr *p = &self_phdrs[i];
        if (p->p_type == PT_LOAD && p->p_vaddr >= 0x400000) {
            if (shadow) die("multiple shadow load segments", 0);
            shadow = p;
        }
    }
    unsigned long current_brk = (unsigned long)sc(SYS_brk, 0, 0, 0);
    if (!missing_shadow) {
        if (!shadow || shadow->p_flags != PF_R || shadow->p_vaddr != start_data ||
            end_data < start_data || shadow->p_filesz != end_data - start_data ||
            shadow->p_memsz != elf_brk - start_data || shadow->p_align != PAGE ||
            POFF(shadow->p_offset) != POFF(start_data))
            die("shadow data geometry mismatch", 0);
        if (current_brk != native_brk) die("shadow brk mismatch", current_brk);
        if (shadow->p_memsz)
            checked_unmap(PSTART(shadow->p_vaddr),
                          PALIGN(POFF(shadow->p_vaddr) + shadow->p_memsz));
    } else if (shadow) {
        die("omit-placeholder mutation unexpectedly has shadow", 0);
    }

    unsigned long phdr_address = 0;
    unsigned long total = total_mapping_size(target_phdrs, target_header.e_phnum);
    int first = 1;
    for (int i = 0; i < target_header.e_phnum; i++) {
        const Elf64_Phdr *p = &target_phdrs[i];
        if (p->p_type != PT_LOAD) continue;
        int flags = MAP_PRIV | (first ? MAP_NOREPLACE : MAP_FIX);
        long mapped = elf_load(TARGET_FD, target_bias + p->p_vaddr, p,
                               prot_of(p->p_flags), flags,
                               first && target_header.e_type == ET_DYN ? total : 0, 0);
        if (is_error(mapped)) die("map target", mapped);
        if (first) {
            first = 0;
            if (target_header.e_type == ET_DYN)
                target_bias += (unsigned long)mapped - PSTART(target_bias + p->p_vaddr);
        }
        if (p->p_offset <= target_header.e_phoff &&
            target_header.e_phoff < p->p_offset + p->p_filesz)
            phdr_address = target_header.e_phoff - p->p_offset + p->p_vaddr;
    }
    phdr_address += target_bias;

    unsigned long interp_bias = 0;
    unsigned long interp_total = total_mapping_size(interp_phdrs, interp_header.e_phnum);
    /* Linux rejects an interpreter without a nonempty total load span even
     * if its entry happens to name memory in the already mapped main image. */
    if (!interp_total) die("interpreter has zero load span", -22);
    int interp_set = 0;
    for (int i = 0; i < interp_header.e_phnum; i++) {
        const Elf64_Phdr *p = &interp_phdrs[i];
        if (p->p_type != PT_LOAD) continue;
        int flags = MAP_PRIV;
        if (interp_set) flags |= MAP_FIX;
        /* load_elf_interp uses MAP_FIXED for a fixed-address interpreter,
         * including legitimate overlap with the already mapped main image.
         * elf_load checks every effective range before replacing mappings. */
        else if (interp_header.e_type == ET_EXEC) flags |= MAP_FIX;
        /* For a zero-bias ET_EXEC main, Linux honors the first ET_DYN
         * interpreter address hint. A biased main supplies no_base instead. */
        else if (target_bias != 0) interp_bias = 0UL - p->p_vaddr;
        else {
            unsigned long hint = PSTART(p->p_vaddr);
            if (p->p_filesz && hint && hint < MIN_PROGRAM_ADDRESS)
                die("interpreter PT_LOAD hint overlaps loader reserved range", -22);
        }
        long mapped = elf_load(interp_fd, interp_bias + p->p_vaddr, p,
                               prot_of(p->p_flags), flags, interp_total, 1);
        interp_total = 0;
        if (is_error(mapped)) die("map interpreter", mapped);
        if (!interp_set && interp_header.e_type == ET_DYN)
            interp_bias = (unsigned long)mapped - PSTART(p->p_vaddr);
        interp_set = 1;
    }

    /* Modular addition is required for supported empty-first negative bias.
     * The target entry auxv is separate; this is the address we actually jump. */
    unsigned long interpreter_entry = interp_bias + interp_header.e_entry;
    if (interpreter_entry < MIN_PROGRAM_ADDRESS || interpreter_entry >= MAP_LIMIT)
        die("interpreter entry outside admitted address range", -22);

    if (!skip_vdso) {
        long slot = sc6(SYS_mmap, 0, special_size, 0, MAP_PRIV | MAP_ANON_VALUE, -1, 0);
        if (is_error(slot)) die("find native vdso slot", slot);
        if ((unsigned long)slot < MIN_PROGRAM_ADDRESS || (unsigned long)slot >= USER_LIMIT ||
            special_size > USER_LIMIT - (unsigned long)slot)
            die("native special mapping overlaps loader reserved range", -22);
        checked_unmap((unsigned long)slot, special_size);
        struct vm moved[3];
        for (int i = 0; i < 3; i++) {
            moved[i].low = temporary_start + special[i].low - special_low;
            moved[i].high = moved[i].low + special[i].high - special[i].low;
        }
        move_special(moved, found, temporary_start, (unsigned long)slot, 0);
        for (Elf64_auxv_t *x = auxv; x->a_type != AT_NULL; x++)
            if (x->a_type == AT_SYSINFO_EHDR)
                x->a_un.a_val = (unsigned long)slot + special[2].low - special_low;
    }
    for (Elf64_auxv_t *x = auxv; x->a_type != AT_NULL; x++) {
        switch (x->a_type) {
        case AT_PHDR: x->a_un.a_val = phdr_address; break;
        case AT_PHNUM: x->a_un.a_val = target_header.e_phnum; break;
        case AT_ENTRY: x->a_un.a_val = target_header.e_entry + target_bias; break;
        case AT_BASE: x->a_un.a_val = interp_bias; break;
        default: break;
        }
    }
    memcpy(execfn, metadata, exec_length + 1);
    long named = sc(SYS_prctl, PR_SET_NAME_VALUE, comm, 0);
    if (is_error(named)) die("set native comm", named);
    checked_close(TARGET_FD);
    checked_close(interp_fd);
    loader_record.sp = (unsigned long)sp;
    loader_record.entry = interpreter_entry;
    loader_enter();
}

#ifdef LOADER_STACK_GUARD_CONTROL
/* A normal test executable calls loader_main's host policy helpers with
 * controlled inputs. No kernel setting or production flag changes. */
void loader_enter(void) { die("unexpected stack guard control handoff", -22); }
int main(int argc, char **argv) {
    const char option[] = "--mmap-min-addr";
    if (argc == 3 && bounded_length(argv[1], sizeof option) == sizeof option - 1 &&
        same_bytes(argv[1], option, sizeof option - 1)) {
        unsigned long minimum = parse_mmap_min_addr(argv[2], bounded_length(argv[2], ~0UL));
        printf("%lu\n", temporary_special_start(minimum));
        return 0;
    }
    if (argc != 2) return 64;
    size_t size = bounded_length(argv[1], sizeof maps_buffer);
    check_stack_guard_cmdline(argv[1], size);
    return 0;
}
#endif
