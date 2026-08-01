/* create a memfd, write "hi" via pwritev2 at offset 0, then read it back and
 * print the round-tripped bytes. Regresses pwritev2 parity under e9patch. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
struct iovec{void*b;unsigned long l;};
void _start(void){ int fd=sc(319,(long)"w2",0,0,0,0,0); /* memfd_create */
 char out[2]={'h','i'}; struct iovec io={out,2};
 sc(328,fd,(long)&io,1,0,0,0); /* pwritev2(fd,&io,1,off=0,flags=0) */
 char in[8]={0}; sc(17,fd,(long)in,8,0,0,0); /* pread at offset 0 */
 puts_("pwritev2="); sc(1,1,(long)in,2,0,0,0); puts_("\n"); die(0); }
