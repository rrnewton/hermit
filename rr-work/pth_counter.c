#include <pthread.h>
#include <stdio.h>
static long c=0; static pthread_mutex_t m=PTHREAD_MUTEX_INITIALIZER;
void* w(void*_){for(int i=0;i<100000;i++){pthread_mutex_lock(&m);c++;pthread_mutex_unlock(&m);}return 0;}
int main(){pthread_t t[4];for(int i=0;i<4;i++)pthread_create(&t[i],0,w,0);for(int i=0;i<4;i++)pthread_join(t[i],0);printf("%ld\n",c);return 0;}
