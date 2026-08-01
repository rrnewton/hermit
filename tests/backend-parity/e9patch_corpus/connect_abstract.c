/* create a listening abstract-namespace AF_UNIX stream socket, then create a
 * second socket and connect() it to that address, printing the connect return
 * (0 on success). Regresses connect parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
struct sa{unsigned short f;char p[14];};
void _start(void){ struct sa a; a.f=1; /* AF_UNIX */
 a.p[0]=0; a.p[1]='c'; a.p[2]='9'; a.p[3]='1'; /* abstract "\0c91" */
 int addrlen=2+4;
 int s1=sc(41,1,1,0,0,0,0); /* socket(AF_UNIX,SOCK_STREAM) */
 sc(49,s1,(long)&a,addrlen,0,0,0); /* bind */
 sc(50,s1,1,0,0,0,0); /* listen backlog 1 */
 int s2=sc(41,1,1,0,0,0,0); /* socket */
 long r=sc(42,s2,(long)&a,addrlen,0,0,0); /* connect */
 sc(3,s2,0,0,0,0,0); sc(3,s1,0,0,0,0,0);
 puts_("connect="); putn(r); die(0); }
