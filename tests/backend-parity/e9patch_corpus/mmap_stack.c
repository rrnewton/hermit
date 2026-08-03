/* anonymous mmap with MAP_STACK (a distinct flag path from mmap_anon and
 * mmap_noreserve; MAP_STACK is a no-op hint on x86-64 but exercises the flag
 * mask). Write a sentinel byte, read it back to confirm the mapping is
 * writable, then munmap. Print the read-back byte (42), host-independent.
 * Regresses mmap flag parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long p=sc(9,0,4096,3,0x20022,-1,0); /* mmap PROT_RW MAP_PRIVATE|ANON|STACK */
 if(p<0) die(1); volatile unsigned char*b=(unsigned char*)p; b[0]=42; unsigned char v=b[0];
 sc(11,p,4096,0,0,0,0); /* munmap */
 puts_("mmapstack="); putn(v); die(0); }
