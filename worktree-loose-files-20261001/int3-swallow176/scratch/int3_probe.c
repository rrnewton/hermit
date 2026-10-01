/* Exactly the issue #1715 reproduction. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
static void i3(void) { __asm__ __volatile__("int3"); }
int main(void) {
    pid_t p = fork();
    if (p == 0) { i3(); _exit(99); }   /* 99 == the trap was swallowed */
    int s = 0; waitpid(p, &s, 0);
    if (WIFSIGNALED(s)) printf("int3: sig=%d\n", WTERMSIG(s));
    else printf("int3: exited=%d (SWALLOWED)\n", WEXITSTATUS(s));
    return 0;
}
