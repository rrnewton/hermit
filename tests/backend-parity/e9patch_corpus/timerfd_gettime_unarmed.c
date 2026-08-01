/* Create a timerfd and query it with timerfd_gettime while it is unarmed: the
 * itimerspec is all zero and the call returns 0 (printed). Only the return is
 * emitted; host-independent. Distinct from timerfd_create_check (creation only)
 * and from the timer_create POSIX-timer family. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long its[4]={0,0,0,0};
 long fd=sc(283,1,0,0,0,0,0); /* timerfd_create(CLOCK_MONOTONIC=1,0) */
 long r=(fd>=0)?sc(287,fd,(long)its,0,0,0,0):fd; /* timerfd_gettime(fd,&its) */
 puts_("timerfdgettime="); putn(r); die(0); }
