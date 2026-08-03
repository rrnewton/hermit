/* create an EFD_SEMAPHORE eventfd, write the counter to 5, then read once and
 * print the value (semaphore reads return 1 and decrement). Distinct flag path
 * from the round-6 plain eventfd guest. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(290,0,1,0,0,0,0); /* eventfd2 EFD_SEMAPHORE=1 */
 unsigned long v=5; sc(1,fd,(long)&v,8,0,0,0); /* write counter=5 */
 unsigned long r=0; sc(0,fd,(long)&r,8,0,0,0); /* read -> 1 */
 sc(3,fd,0,0,0,0,0); puts_("efdsem="); putn((long)r); die(0); }
