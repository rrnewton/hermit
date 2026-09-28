/* A forked child that execs after the session root has exited.

   Exec after start cannot preserve the preload runtime. The child waits until
   the kernel has reparented it, proving that the root exited before the
   refused exec. The root would then wait forever for the child's Tool exit
   callback if its process-exit bookkeeping ignored the session failure. */
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/types.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc != 3 || prctl(PR_SET_NAME, argv[1], 0, 0, 0) != 0) {
    return 9;
  }
  FILE *pid_file = fopen(argv[2], "w");
  if (pid_file == NULL) {
    return 8;
  }
  fprintf(pid_file, "%ld\n", (long)getpid());
  if (fclose(pid_file) != 0) {
    return 7;
  }

  pid_t root = getpid();
  pid_t child = fork();
  if (child < 0) {
    return 10;
  }
  if (child == 0) {
    while (getppid() == root) {
      usleep(1000);
    }
    execl("/bin/true", "true", (char *)NULL);
    _exit(127);
  }

  puts("fork-exec-root-finished");
  return 0;
}
