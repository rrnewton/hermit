/* A libc program whose layout can be compared with an ordinary kernel exec.
 * Output is KEY=VALUE.  Scalars use decimal except addresses (0x-prefixed
 * hexadecimal); arbitrary bytes use lower-case hexadecimal without 0x.
 * Printing uses write(2), so collecting the initial brk and mappings does
 * not accidentally introduce stdio's malloc allocation.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <link.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

extern uintptr_t fixture_entry_rsp;
extern size_t fixture_stack_len;
extern unsigned char fixture_stack[];

static unsigned char file_bytes[65536];
static uintptr_t program_base;
static uintptr_t libc_base;

static void die(const char *message) {
    (void)write(STDERR_FILENO, message, strlen(message));
    _exit(96);
}

static void output(const void *bytes, size_t length) {
    const unsigned char *next = bytes;
    while (length) {
        ssize_t written = write(STDOUT_FILENO, next, length);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) die("layout fixture: write failed\n");
        next += (size_t)written;
        length -= (size_t)written;
    }
}

static void key_start(const char *key) {
    output(key, strlen(key));
    output("=", 1);
}

static void number(const char *key, uint64_t value, unsigned int radix) {
    static const char digits[] = "0123456789abcdef";
    char buffer[24];
    char *last = buffer + sizeof buffer;
    char *first = last;
    do {
        *--first = digits[value % radix];
        value /= radix;
    } while (value);
    key_start(key);
    if (radix == 16) output("0x", 2);
    output(first, (size_t)(last - first));
    output("\n", 1);
}

static void hex_bytes(const char *key, const void *bytes, size_t length) {
    static const char digits[] = "0123456789abcdef";
    const unsigned char *data = bytes;
    char chunk[4096];
    key_start(key);
    while (length) {
        size_t take = length < sizeof chunk / 2 ? length : sizeof chunk / 2;
        for (size_t i = 0; i < take; i++) {
            chunk[2 * i] = digits[data[i] >> 4];
            chunk[2 * i + 1] = digits[data[i] & 15];
        }
        output(chunk, take * 2);
        data += take;
        length -= take;
    }
    output("\n", 1);
}

static size_t read_file(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) die("layout fixture: proc open failed\n");
    size_t used = 0;
    for (;;) {
        ssize_t count = read(fd, file_bytes + used, sizeof file_bytes - used);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) die("layout fixture: proc read failed\n");
        if (!count) break;
        used += (size_t)count;
        if (used == sizeof file_bytes) die("layout fixture: proc file too large\n");
    }
    if (close(fd)) die("layout fixture: proc close failed\n");
    return used;
}

static uint64_t data_kib(void) {
    size_t length = read_file("/proc/self/status");
    const char *line = (const char *)file_bytes;
    const char *end = line + length;
    while (line < end) {
        if ((size_t)(end - line) >= 7 && !memcmp(line, "VmData:", 7)) {
            line += 7;
            while (line < end && (*line == ' ' || *line == '\t')) line++;
            uint64_t value = 0;
            while (line < end && *line >= '0' && *line <= '9')
                value = value * 10 + (unsigned int)(*line++ - '0');
            return value;
        }
        while (line < end && *line != '\n') line++;
        if (line < end) line++;
    }
    die("layout fixture: VmData missing\n");
    return 0;
}

static void indexed_key(char *out, const char *prefix, size_t index,
                        const char *suffix) {
    while (*prefix) *out++ = *prefix++;
    char reversed[24];
    size_t digits = 0;
    do {
        reversed[digits++] = (char)('0' + index % 10);
        index /= 10;
    } while (index);
    while (digits) *out++ = reversed[--digits];
    while (*suffix) *out++ = *suffix++;
    *out = 0;
}

static int bases(struct dl_phdr_info *info, size_t size, void *unused) {
    (void)size;
    (void)unused;
    if (!info->dlpi_name[0]) program_base = (uintptr_t)info->dlpi_addr;
    const char *base = strrchr(info->dlpi_name, '/');
    base = base ? base + 1 : info->dlpi_name;
    if (!strncmp(base, "libc.so", 7) || !strncmp(base, "libc-", 5))
        libc_base = (uintptr_t)info->dlpi_addr;
    return 0;
}

static void proc_stat(void) {
    size_t length = read_file("/proc/self/stat");
    char *last_paren = NULL;
    for (size_t i = 0; i < length; i++)
        if (file_bytes[i] == ')') last_paren = (char *)file_bytes + i;
    if (!last_paren) die("layout fixture: malformed stat\n");
    const char *token = last_paren + 2;
    const char *end = (const char *)file_bytes + length;
    unsigned int field = 3;
    while (token < end) {
        const char *next = token;
        while (next < end && *next != ' ' && *next != '\n') next++;
        if ((field >= 26 && field <= 28) || (field >= 45 && field <= 51)) {
            char key[24];
            indexed_key(key, "stat.", field, "");
            key_start(key);
            output(token, (size_t)(next - token));
            output("\n", 1);
        }
        token = next;
        while (token < end && (*token == ' ' || *token == '\n')) token++;
        field++;
    }
}

int main(void) {
    const uintptr_t initial_brk = (uintptr_t)syscall(SYS_brk, 0);
    const uint64_t baseline_data = data_kib();
    uint64_t peak_data = baseline_data;
    size_t limit_pages = 16;
    const char *limit = getenv("ELF_LOADER_HEAP_PAGES");
    if (limit) {
        limit_pages = 0;
        while (*limit >= '0' && *limit <= '9') {
            limit_pages = limit_pages * 10 + (unsigned int)(*limit++ - '0');
            if (limit_pages > 4096) die("layout fixture: heap probe too large\n");
        }
        if (*limit || !limit_pages) die("layout fixture: invalid heap probe size\n");
    }
    size_t successful_pages = 0;
    uintptr_t failed_request = 0, failed_result = 0;
    for (size_t i = 1; i <= limit_pages; i++) {
        uintptr_t request = initial_brk + i * 4096;
        uintptr_t result = (uintptr_t)syscall(SYS_brk, request);
        if (result != request) {
            failed_request = request;
            failed_result = result;
            break;
        }
        successful_pages++;
        uint64_t current_data = data_kib();
        if (current_data > peak_data) peak_data = current_data;
    }
    /* Leave a real [heap] VMA for the mapping comparison. */
    const uintptr_t final_brk = initial_brk + (successful_pages ? 4096 : 0);
    if ((uintptr_t)syscall(SYS_brk, final_brk) != final_brk)
        die("layout fixture: brk restoration failed\n");

    number("entry.rsp", fixture_entry_rsp, 16);
    number("entry.stack_start", fixture_entry_rsp, 16);
    number("entry.stack_len", fixture_stack_len, 10);
    hex_bytes("entry.stack", fixture_stack, fixture_stack_len);
    const uintptr_t *words = (const uintptr_t *)fixture_stack;
    const size_t argc = words[0];
    number("argc", argc, 10);
    size_t pos = 1;
    char key[48];
    for (size_t i = 0; i < argc; i++, pos++) {
        indexed_key(key, "argv.", i, ".ptr");
        number(key, words[pos], 16);
        indexed_key(key, "argv.", i, ".hex");
        hex_bytes(key, (const void *)words[pos], strlen((const char *)words[pos]));
    }
    if (words[pos++]) die("layout fixture: argv terminator nonzero\n");
    size_t envc = 0;
    while (words[pos + envc]) envc++;
    number("envc", envc, 10);
    for (size_t i = 0; i < envc; i++, pos++) {
        indexed_key(key, "env.", i, ".ptr");
        number(key, words[pos], 16);
        indexed_key(key, "env.", i, ".hex");
        hex_bytes(key, (const void *)words[pos], strlen((const char *)words[pos]));
    }
    pos++; /* envp NULL */
    const Elf64_auxv_t *auxv = (const Elf64_auxv_t *)(words + pos);
    size_t auxc = 0;
    while (auxv[auxc++].a_type != AT_NULL) {}
    number("auxc", auxc, 10);
    uintptr_t execfn = 0, interpreter_base = 0;
    for (size_t i = 0; i < auxc; i++) {
        indexed_key(key, "aux.", i, ".type");
        number(key, auxv[i].a_type, 10);
        indexed_key(key, "aux.", i, ".value");
        number(key, auxv[i].a_un.a_val, 16);
        if (auxv[i].a_type == AT_EXECFN) execfn = auxv[i].a_un.a_val;
        if (auxv[i].a_type == AT_BASE) interpreter_base = auxv[i].a_un.a_val;
    }
    if (!execfn) die("layout fixture: execfn missing\n");
    number("execfn.ptr", execfn, 16);
    hex_bytes("execfn.hex", (const void *)execfn, strlen((const char *)execfn));
    (void)dl_iterate_phdr(bases, NULL);
    if (!libc_base) die("layout fixture: libc missing\n");
    number("program.base", program_base, 16);
    number("interpreter.base", interpreter_base, 16);
    number("libc.base", libc_base, 16);
    number("heap.start", initial_brk, 16);
    number("heap.end", final_brk, 16);
    number("heap.probe_limit_pages", limit_pages, 10);
    number("heap.growth_pages", successful_pages, 10);
    number("heap.first_failed_request", failed_request, 16);
    number("heap.first_failed_result", failed_result, 16);
    number("data.baseline_kib", baseline_data, 10);
    number("data.peak_kib", peak_data, 10);
    proc_stat();
    size_t length = read_file("/proc/self/comm");
    hex_bytes("comm", file_bytes, length);
    length = read_file("/proc/self/cmdline");
    hex_bytes("cmdline", file_bytes, length);
    ssize_t link_length = readlink("/proc/self/exe", (char *)file_bytes, sizeof file_bytes);
    if (link_length < 0 || (size_t)link_length == sizeof file_bytes)
        die("layout fixture: exe readlink failed\n");
    hex_bytes("exe", file_bytes, (size_t)link_length);
    length = read_file("/proc/self/auxv");
    hex_bytes("proc_auxv", file_bytes, length);
    length = read_file("/proc/self/maps");
    hex_bytes("maps", file_bytes, length);
    return 0;
}
