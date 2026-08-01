/* memory-advice: mmap an anonymous page, touch it, MADV_DONTNEED it, then read
 * it back as zero-filled. Regresses madvise parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long p=sc(9,0,4096,3,34,-1,0); int ok=!(p<0&&p>-4096); char*m=(char*)p; if(ok) m[0]='X'; long a=sc(28,p,4096,4,0,0,0); int zero = ok && a==0 && m[0]==0; sc(11,p,4096,0,0,0,0); puts_("madvise="); puts_(zero?"ok\n":"fail\n"); die(0); }
