#include <stdio.h>
#include <sys/auxv.h>
#include <unistd.h>
int main(void) {
  unsigned char *r = (unsigned char *)getauxval(AT_RANDOM);
  printf("AT_RANDOM=");
  if (!r) { printf("NULL\n"); } else {
    for (int i = 0; i < 16; i++) printf("%02x", r[i]);
    printf("\n");
  }
  printf("getpid=%d\n", (int)getpid());
  return 0;
}
