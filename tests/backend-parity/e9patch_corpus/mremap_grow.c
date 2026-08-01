/* grow an anonymous mapping from one to two pages with mremap(MREMAP_MAYMOVE),
 * write into the new tail, and read it back. Regresses mremap parity under
 * e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long p=sc(9,0,4096,3,0x22,-1,0); long q=sc(25,p,4096,8192,1,0,0); char*m=(char*)q; m[5000]=0x59; char got=m[5000]; sc(11,q,8192,0,0,0,0); if(got==0x59) puts_("mremap=ok\n"); else puts_("mremap=bad\n"); die(0); }
