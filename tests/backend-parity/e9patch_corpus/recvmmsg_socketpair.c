/* create an AF_UNIX socketpair, sendmmsg one datagram of "hi", then recvmmsg it
 * back into a buffer and print the received bytes. The receive-side batch
 * counterpart to sendmmsg_socketpair. Regresses recvmmsg parity under e9patch. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]=0x0a;if(!u)b[i--]=0x30;while(u){b[i--]=0x30+(u%10);u/=10;}if(v<0)b[i--]=0x2d;sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
struct iovec{void*b;unsigned long l;};
struct msghdr{void*n;unsigned int nl;int _p;struct iovec*iov;unsigned long iovl;void*c;unsigned long cl;int fl;};
struct mmsghdr{struct msghdr h;unsigned int len;};
void _start(void){ int sv[2]={-1,-1};
 sc(53,1,2,0,(long)sv,0,0); /* socketpair AF_UNIX SOCK_DGRAM */
 char out[2]={'h','i'}; struct iovec io={out,2};
 struct mmsghdr sm; for(unsigned i=0;i<sizeof sm;i++) ((char*)&sm)[i]=0;
 sm.h.iov=&io; sm.h.iovl=1;
 sc(307,sv[0],(long)&sm,1,0,0,0); /* sendmmsg one msg */
 char in[8]={0}; struct iovec io2={in,8};
 struct mmsghdr rm; for(unsigned i=0;i<sizeof rm;i++) ((char*)&rm)[i]=0;
 rm.h.iov=&io2; rm.h.iovl=1;
 sc(299,sv[1],(long)&rm,1,0,0,0); /* recvmmsg (NULL timeout) */
 sc(3,sv[0],0,0,0,0,0); sc(3,sv[1],0,0,0,0,0);
 puts_("recvmmsg="); sc(1,1,(long)in,2,0,0,0); puts_("\n"); die(0); }
