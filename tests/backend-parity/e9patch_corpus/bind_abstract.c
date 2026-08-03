/* bind an AF_UNIX/SOCK_STREAM socket to an abstract-namespace address (leading
 * NUL, no filesystem entry), printing the return (0 on success). sockaddr_un:
 * family u16@0, then abstract path bytes (sun_path[0]=0). */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int s=sc(41,1,1,0,0,0,0); /* socket AF_UNIX=1 SOCK_STREAM=1 */
 unsigned char sa[16]; for(int i=0;i<16;i++) sa[i]=0;
 *(unsigned short*)&sa[0]=1; /* sun_family=AF_UNIX */
 sa[2]=0; sa[3]='h'; sa[4]='r'; sa[5]='1'; sa[6]='7'; /* abstract "\0hr17" */
 long r=sc(49,s,(long)sa,7,0,0,0); /* bind, addrlen=2+5 */
 sc(3,s,0,0,0,0,0); puts_("bind="); putn(r); die(0); }
