#include <unistd.h>
int main(void){ const char m[]="hello world\n"; write(1,m,sizeof(m)-1); return 0; }
