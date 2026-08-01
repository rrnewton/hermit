/* Create a POSIX per-process timer (timer_create, CLOCK_MONOTONIC, default
 * SIGALRM notification) and query it with timer_gettime. The timer is never
 * armed, so timer_gettime reports a zeroed itimerspec and returns 0 (printed).
 * Only the return is emitted; host-independent. Distinct from timerfd_create. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long tid=0; long its[4]={0,0,0,0};
 long c=sc(222,1,0,(long)&tid,0,0,0); /* timer_create(CLOCK_MONOTONIC=1,NULL,&tid) */
 long r=(c==0)?sc(224,tid,(long)its,0,0,0,0):c; /* timer_gettime(tid,&its) */
 puts_("timergettime="); putn(r); die(0); }
