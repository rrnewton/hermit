/* dense_syscalls: two SYSCALL instructions back-to-back with NO instruction
 * between them (encoded 0F 05 0F 05, four bytes total), forcing e9tool to plant
 * two independent trampolines whose 5-byte control-transfers overlap the very
 * next 2-byte site. This is the canonical adjacent-short-instruction /
 * "straddler" relocation stress for the AOT rewrite engine: unlike every other
 * corpus guest, the two rewritten sites are lexically adjacent rather than each
 * wrapped in its own function.
 *
 * Both syscalls are write(1, &c, 1). The second reuses the register state left
 * by the first: SYSCALL clobbers only rax/rcx/r11, so rdi/rsi/rdx survive, and
 * write() of one byte returns 1 == SYS_write, so rax is already 1 for the second
 * call. Output is therefore the byte twice, then a newline, giving "zz\n".
 * Deterministic: no time, randomness, pid, or scheduling is observed.
 */
static char c = 'z';

void _start(void) {
    long r;
    __asm__ volatile(
        "syscall\n\t"
        "syscall"
        : "=a"(r)
        : "a"(1L), "D"(1L), "S"(&c), "d"(1L)
        : "rcx", "r11", "memory");

    const char nl = '\n';
    long w;
    __asm__ volatile("syscall"
                     : "=a"(w)
                     : "a"(1L), "D"(1L), "S"(&nl), "d"(1L)
                     : "rcx", "r11", "memory");

    __asm__ volatile("syscall" ::"a"(231L), "D"(0L) : "rcx", "r11", "memory");
    __builtin_unreachable();
}
