/* query a would-be write lock on an unlocked memfd with fcntl F_GETLK; the
 * kernel reports the lock as available by rewriting l_type to F_UNLCK (2).
 * Regresses fcntl(F_GETLK) parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(319,(long)"lk",0,0,0,0,0); /* memfd_create */
 sc(77,fd,4096,0,0,0,0); /* ftruncate */
 short fl[16]; for(int i=0;i<16;i++) fl[i]=0;
 fl[0]=1; /* l_type=F_WRLCK */ fl[1]=0; /* l_whence=SEEK_SET */
 sc(72,fd,5,(long)fl,0,0,0); /* fcntl F_GETLK */
 sc(3,fd,0,0,0,0,0); puts_("getlk="); putn(fl[0]); die(0); }
