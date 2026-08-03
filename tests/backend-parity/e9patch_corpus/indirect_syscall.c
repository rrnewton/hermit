/* indirect_syscall: the rewritten SYSCALL site is reached through an indirect
 * (computed) call via a volatile function pointer rather than a direct call.
 * Every other corpus guest reaches its site through a direct call the compiler
 * can see; here the call target is opaque at compile time, exercising that
 * e9tool's ahead-of-time site rewrite is honored when control reaches the site
 * through an indirect branch. The `volatile` qualifier prevents the compiler
 * from devirtualizing the pointer back into a direct call.
 *
 * Writes "indirect\n". Deterministic: no time, randomness, pid, or scheduling is
 * observed.
 */
static long __attribute__((noinline)) do_write(const char *m, long n) {
    long r;
    __asm__ volatile("syscall"
                     : "=a"(r)
                     : "a"(1L), "D"(1L), "S"(m), "d"(n)
                     : "rcx", "r11", "memory");
    return r;
}

static void __attribute__((noinline, noreturn)) do_exit(long c) {
    __asm__ volatile("syscall" ::"a"(231L), "D"(c) : "rcx", "r11", "memory");
    __builtin_unreachable();
}

static long (*volatile write_fp)(const char *, long) = do_write;

void _start(void) {
    const char m[] = "indirect\n";
    write_fp(m, 9);
    do_exit(0);
}
