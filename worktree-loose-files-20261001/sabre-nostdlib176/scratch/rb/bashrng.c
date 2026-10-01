/* bash 5.1 variables.c: intrand32 (Park-Miller) + brand + seedrand */
#include <stdio.h>
#include <stdlib.h>
typedef unsigned int u32; typedef int s32;
static u32 intrand32(u32 last){ s32 h,l,t; u32 r = last?last:123459876u;
  h = r/127773u; l = r - 127773u*h; t = 16807*l - 2836*h;
  return (t<0)? (u32)(t+0x7fffffff) : (u32)t; }
static u32 rseed; static int lastv;
static int brand(void){ rseed = intrand32(rseed); return (int)(((rseed>>16) ^ (rseed & 65535)) & 32767); }
static int getrand(void){ int rv; do rv = brand(); while (rv == lastv); return (lastv = rv); }
int main(int argc,char**argv){
  /* argv: tv_sec pid r1 r2 r3   -> search tv_usec that reproduces r1,r2,r3 */
  long sec = atol(argv[1]); long pid = atol(argv[2]);
  int t1=atoi(argv[3]), t2=atoi(argv[4]), t3=atoi(argv[5]);
  for (long us=0; us<1000000; us++){
    rseed = (u32)(sec ^ us ^ pid); lastv = 0;
    if (getrand()==t1 && getrand()==t2 && getrand()==t3){ printf("MATCH tv_usec=%ld seed=%u\n", us, (u32)(sec^us^pid)); return 0; }
  }
  printf("no match\n"); return 1;
}
