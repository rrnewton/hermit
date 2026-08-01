/* poll an empty epoll interest set with the timespec-based epoll_pwait2(441) and
 * a zero timeout, distinct from the epoll_pwait(281) guest that takes a
 * millisecond timeout. A zero-timeout poll returns 0 ready events immediately
 * without blocking or registering a timed waiter. Prints the ready-event count
 * (0), a host-independent value. Regresses epoll_pwait2 parity under e9patch
 * preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long epfd=sc(291,0,0,0,0,0,0); /* epoll_create1(0) */
 char ev[16]; long ts[2]={0,0}; /* struct timespec {0,0} */
 long r=sc(441,epfd,(long)ev,1,(long)ts,0,8); /* epoll_pwait2(...,NULL sigmask,8) */
 puts_("epollpwait2="); putn(r); die(0); }
