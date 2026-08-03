/* create a packet-mode pipe with pipe2(O_DIRECT), write two bytes and read them
 * back as one packet; the round-trip yields the fixed bytes "hi", a
 * host-independent constant. A distinct pipe-flag family from the plain pipe,
 * pipe2(O_NONBLOCK), and legacy pipe guests, exercising pipe2 parity under
 * e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int pf[2]; sc(293,(long)pf,0x4000,0,0,0,0); /* pipe2 O_DIRECT=0x4000 */
 char w[2]={'h','i'}; sc(1,pf[1],(long)w,2,0,0,0);
 char r[3]={0,0,0}; sc(0,pf[0],(long)r,2,0,0,0);
 sc(3,pf[0],0,0,0,0,0); sc(3,pf[1],0,0,0,0,0);
 puts_("pd="); puts_(r); sc(1,1,(long)"\n",1,0,0,0); die(0); }
