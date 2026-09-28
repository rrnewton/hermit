/* A forked child that execs. Exec after start cannot preserve the preload
   runtime, so the child fails closed -- in a task that has no outer cleanup
   guard of its own. The root goes on to exit zero, so the session must refuse
   to report that success: this fixture exists to make a silent green
   impossible, not to be supported. */
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/types.h>
#include <sys/wait.h>
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
  pid_t child = fork();
  if (child < 0) {
    return 10;
  }
  if (child == 0) {
    execl("/bin/true", "true", (char *)NULL);
    _exit(127);
  }
  int status = 0;
  (void)waitpid(child, &status, 0);
  puts("fork-exec-root-finished");
  return 0;
}
