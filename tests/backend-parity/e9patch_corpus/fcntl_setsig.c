/* set the I/O-availability signal on a memfd with fcntl(F_SETSIG, SIGUSR1) then
 * read it back with F_GETSIG; the value round-trips to SIGUSR1 (10), a
 * host-independent constant. No signal is ever delivered (no O_ASYNC owner set).
 * A distinct fcntl OP from the F_GETSIG-only guest, exercising fcntl parity under
 * e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(319,(long)"m",0,0,0,0,0); /* memfd_create */
 sc(72,fd,10,10,0,0,0); /* fcntl F_SETSIG=10, SIGUSR1=10 */
 long g=sc(72,fd,11,0,0,0,0); /* fcntl F_GETSIG=11 */
 sc(3,fd,0,0,0,0,0);
 puts_("setsig="); putn(g); die(0); }
