#include <stdio.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#include <sys/auxv.h>
int main(void) {
  struct timeval tv; gettimeofday(&tv, NULL);
  struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
  unsigned char *r = (unsigned char *)getauxval(AT_RANDOM);
  printf("tv=%ld.%06ld ts=%ld.%09ld pid=%d seed=%ld atr=%02x%02x\n",
         (long)tv.tv_sec, (long)tv.tv_usec, (long)ts.tv_sec, (long)ts.tv_nsec,
         (int)getpid(), (long)(tv.tv_sec ^ tv.tv_usec ^ getpid()), r?r[0]:0, r?r[1]:0);
  return 0;
}
