/* query SO_TIMESTAMPNS on a fresh UDP socket; nanosecond receive timestamping is
 * off by default, so the option reads back 0, a host-independent constant. The
 * socket is created and immediately closed with no bind or send, so nothing
 * communicates. A distinct socket option from the SO_TIMESTAMP guest, exercising
 * getsockopt parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int s=sc(41,2,2,0,0,0,0); /* socket AF_INET,SOCK_DGRAM */
 int v=0; int l=4; long r=sc(55,s,1,35,(long)&v,(long)&l,0); /* getsockopt SOL_SOCKET,SO_TIMESTAMPNS=35 */
 sc(3,s,0,0,0,0,0);
 puts_("timestampns="); putn(r<0?r:v); die(0); }
