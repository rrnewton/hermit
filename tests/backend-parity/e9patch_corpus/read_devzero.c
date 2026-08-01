/* content-I/O: reading 16 bytes from /dev/zero yields 16 zero bytes. Regresses
 * that e9patch preprocessing does not perturb read() content or length. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long fd=sc(2,(long)"/dev/zero",0,0,0,0,0); char b[16]; for(int i=0;i<16;i++) b[i]=0x55; long n=sc(0,fd,(long)b,16,0,0,0); long z=0; for(long i=0;i<n;i++) if(b[i]==0) z++; sc(3,fd,0,0,0,0,0); puts_("zeros="); putn(z); die(0); }
