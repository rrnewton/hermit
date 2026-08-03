/* identity: getuid/geteuid/getgid/getegid. Absolute values are host-specific,
 * so the driver asserts golden==e9patch parity only (expected_stdout=None);
 * regresses that e9patch preprocessing does not perturb credential syscalls. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long uid=sc(102,0,0,0,0,0,0); long euid=sc(107,0,0,0,0,0,0); long gid=sc(104,0,0,0,0,0,0); long egid=sc(108,0,0,0,0,0,0); puts_("uid="); putn(uid); puts_("euid="); putn(euid); puts_("gid="); putn(gid); puts_("egid="); putn(egid); die(0); }
