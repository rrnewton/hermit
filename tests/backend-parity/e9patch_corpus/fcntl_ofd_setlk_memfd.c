/* place a whole-file OFD unlock (F_OFD_SETLK with l_type=F_UNLCK) on an unlocked
 * memfd; clearing a non-existent lock succeeds and returns 0, a host-independent
 * constant. Exercises the open-file-description lock SET path, distinct from the
 * F_OFD_GETLK query guest and from the process-associated F_SETLK guest; OFD
 * locks require l_pid==0. Regresses fcntl parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int fd=sc(319,(long)"m",0,0,0,0,0); sc(77,fd,64,0,0,0,0); /* ftruncate */
 short fl[16]={0}; fl[0]=2; /* struct flock: l_type=F_UNLCK=2, rest 0 (whole file, l_pid=0) */
 long v=sc(72,fd,37,(long)fl,0,0,0); /* fcntl F_OFD_SETLK=37 */
 sc(3,fd,0,0,0,0,0);
 puts_("ofdsetlk="); putn(v); die(0); }
