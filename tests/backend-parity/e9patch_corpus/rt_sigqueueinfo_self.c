/* queue signal 0 to this process via rt_sigqueueinfo with an SI_QUEUE siginfo;
 * signal 0 performs only the permission check (no delivery) and returns 0,
 * which is printed. Regresses rt_sigqueueinfo parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long pid=sc(39,0,0,0,0,0,0); /* getpid */
 char si[128]; for(int i=0;i<128;i++) si[i]=0; *(int*)(si+8)=-1; /* si_code=SI_QUEUE */
 long r=sc(129,pid,0,(long)si,0,0,0); /* rt_sigqueueinfo(tgid,sig=0,&si) */
 puts_("sigqueueinfo="); putn(r); die(0); }
