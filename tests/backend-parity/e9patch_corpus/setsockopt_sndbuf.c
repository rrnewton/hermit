/* set SO_SNDBUF on a fresh TCP socket with setsockopt, then confirm with
 * getsockopt; the kernel-adjusted buffer size is host-variable so only the
 * boolean "readback > 0" (1) is printed, a host-independent constant. Distinct
 * from getsockopt_sndbuf (query only). */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(41,2,1,0,0,0,0); int val=8192; sc(54,fd,1,7,(long)&val,4,0); int v=0; int l=4; sc(55,fd,1,7,(long)&v,(long)&l,0); sc(3,fd,0,0,0,0,0); puts_("setsndbuf="); putn(v>0?1:0); die(0); }
