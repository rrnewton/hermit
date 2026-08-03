/* IP_MTU_DISCOVER(10) at IPPROTO_IP(0) to IP_PMTUDISC_DO(2) on an AF_INET/SOCK_STREAM socket. Set the option to 2 with setsockopt(54), read it back with getsockopt(55),
 * and print the round-tripped value; the kernel echoes exactly what was set, a
 * host-independent constant identical native and golden. Distinct write path
 * from any read-only getsockopt guest. Regresses setsockopt parity under e9patch
 * preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(41,2,1,0,0,0,0);
 int v=2; sc(54,fd,0,10,(long)&v,4,0); /* setsockopt */
 int g=-1; int l=4; long r=sc(55,fd,0,10,(long)&g,(long)&l,0); /* getsockopt */
 sc(3,fd,0,0,0,0,0); puts_("setipmtudisc="); putn(r==0?g:r); die(0); }
