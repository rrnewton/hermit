/* map an anonymous page, revoke all access with mprotect(PROT_NONE), then restore
 * PROT_READ; both mprotect calls return 0, so the guest prints 0, a
 * host-independent constant. A distinct protection transition from the
 * RW->RO->RW mprotect_roundtrip guest (this one drops to no-access and back),
 * exercising mprotect parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long a=sc(9,0,4096,3,0x22,-1,0); /* mmap PROT_RW MAP_PRIVATE|ANON */
 long r1=sc(10,a,4096,0,0,0,0); /* mprotect PROT_NONE=0 */
 long r2=sc(10,a,4096,1,0,0,0); /* mprotect PROT_READ=1 */
 sc(11,a,4096,0,0,0,0);
 puts_("protnone="); putn((r1==0&&r2==0)?0:-1); die(0); }
