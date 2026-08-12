#include <unistd.h>
int main(void){for(int i=0;i<3;i++) getpid(); write(1,"x\n",2); return 0;}
