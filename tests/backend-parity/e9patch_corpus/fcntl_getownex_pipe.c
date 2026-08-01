/* query the extended owner of a pipe read end with fcntl F_GETOWN_EX; a pipe with
 * no owner set reports an f_owner_ex whose type field is 0 (F_OWNER_TID/PID/PGRP
 * all unset), a host-independent constant. A distinct fcntl OP from the plain
 * F_GETOWN guest (which returns the owner as a scalar), exercising the struct
 * copyout path. Regresses fcntl parity under e9patch preprocessing. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ int pf[2]; sc(22,(long)pf,0,0,0,0,0); /* pipe */
 long ox[2]={-1,-1}; /* struct f_owner_ex { int type; pid_t pid; } */
 sc(72,pf[0],16,(long)ox,0,0,0); /* fcntl F_GETOWN_EX=16 */
 sc(3,pf[0],0,0,0,0,0); sc(3,pf[1],0,0,0,0,0);
 puts_("getownex="); putn((int)ox[0]); die(0); }
