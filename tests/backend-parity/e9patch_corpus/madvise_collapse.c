/* apply MADV_COLLAPSE=25 to a small private anonymous mapping with madvise(28);
 * a 4 KiB anon range is not THP-collapsible so the kernel returns -EINVAL(-22)
 * both natively and under golden hermit ptrace, a host-independent constant.
 * This is faithful Linux parity (native == golden), not a hermit limitation.
 * Regresses madvise error-path parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long m=sc(9,0,4096,3,0x22,-1,0); /* mmap PRIVATE|ANON RW */
 long v=sc(28,m,4096,25,0,0,0); /* madvise MADV_COLLAPSE=25 -> -EINVAL */
 sc(11,m,4096,0,0,0,0); /* munmap */
 puts_("madvcollapse="); putn(v); die(0); }
