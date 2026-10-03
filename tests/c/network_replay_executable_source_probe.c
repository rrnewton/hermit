/* Component fixture for the real ptrace executable-source capture path.
 * No libc startup, socket, mutation, or alternative payload is involved. */
#if !defined(__x86_64__)
#error "This native full-register fixture is x86_64-only"
#endif
static const unsigned char source[8] __attribute__((aligned(4096))) =
    {'n', 'e', 'x', 't', '\n', 'd', 'o', 'n'};

__attribute__((noreturn)) void _start(void) {
    register long nr __asm__("rax") = 44; /* __NR_sendto */
    register long fd __asm__("rdi") = 7;
    register const unsigned char *buffer __asm__("rsi") = source;
    register long count __asm__("rdx") = sizeof(source);
    register long flags __asm__("r10") = 0x4000; /* MSG_NOSIGNAL */
    register long address __asm__("r8") = 0;
    register long address_length __asm__("r9") = 0;
    __asm__ volatile("syscall" : "+a"(nr)
                     : "D"(fd), "S"(buffer), "d"(count), "r"(flags),
                       "r"(address), "r"(address_length)
                     : "rcx", "r11", "memory");
    register long status __asm__("rdi") = nr == (long)sizeof(source) ? 0 : 91;
    nr = 231; /* __NR_exit_group */
    __asm__ volatile("syscall" : "+a"(nr) : "D"(status)
                     : "rcx", "r11", "memory");
    __builtin_unreachable();
}
