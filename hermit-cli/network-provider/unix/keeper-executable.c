/* SPDX-License-Identifier: GPL-2.0 */
#include "keeper-channel.h"
#include <errno.h>
#include <stdlib.h>
#include <string.h>
/* Exec-created helper. The parent computes this absolute deadline before
 * spawning the capability launcher; retained transport aliases cannot renew it. */
int main(int argc,char **argv) {
    if(argc!=3 || strcmp(argv[1],"--bootstrap-deadline-ns") || !argv[2][0])return 125;
    for(const char *p=argv[2];*p;p++)if(*p<'0' || *p>'9')return 125;
    char *end=NULL;errno=0;
    unsigned long long deadline=strtoull(argv[2],&end,10);
    if(errno || !end || *end || !deadline)return 125;
    return ug_keeper_main((u64)deadline);
}
