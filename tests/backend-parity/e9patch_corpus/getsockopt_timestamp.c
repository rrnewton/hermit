/* query SO_TIMESTAMP on a fresh AF_UNIX socketpair endpoint with getsockopt;
 * receive timestamping is disabled by default, so the returned flag is 0, a
 * host-independent constant (a distinct boolean OPTION from the other SO_* reads
 * already covered). Regresses getsockopt parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int sv[2]; sc(53,1,1,0,(long)sv,0,0); /* socketpair AF_UNIX SOCK_STREAM */
 int val=-1; int len=4;
 sc(55,sv[0],1,29,(long)&val,(long)&len,0); /* getsockopt SOL_SOCKET SO_TIMESTAMP */
 sc(3,sv[0],0,0,0,0,0); sc(3,sv[1],0,0,0,0,0);
 puts_("timestamp="); putn(val); die(0); }
