/* read IPV6_UNICAST_HOPS on a fresh AF_INET6/SOCK_STREAM socket; the default
 * unicast hop limit is 64, read back with getsockopt at the IPPROTO_IPV6
 * protocol level (a new level distinct from all prior SOL_SOCKET guests), a
 * host-independent constant, so e9patch preprocessing leaves it byte-identical. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(41,10,1,0,0,0,0); int v=-99; int l=4; long r=sc(55,fd,41,16,(long)&v,(long)&l,0); sc(3,fd,0,0,0,0,0); puts_("v6hops="); putn(r==0?v:r); die(0); }
