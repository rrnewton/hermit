/* resize a pipe with fcntl F_SETPIPE_SZ; requesting the page size (4096) returns
 * the granted capacity, which the kernel rounds to at least one page, so a 4096
 * request is granted exactly 4096, a host-independent constant. A distinct fcntl
 * OP from the read-only F_GETPIPE_SZ guest, exercising the resize (write) path.
 * Regresses fcntl parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int pf[2]; sc(22,(long)pf,0,0,0,0,0); /* pipe */
 long v=sc(72,pf[0],1031,4096,0,0,0); /* fcntl F_SETPIPE_SZ=1031, request 4096 */
 sc(3,pf[0],0,0,0,0,0); sc(3,pf[1],0,0,0,0,0);
 puts_("setpipesz="); putn(v); die(0); }
