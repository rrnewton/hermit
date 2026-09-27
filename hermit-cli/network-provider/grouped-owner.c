/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#include "grouped-owner.h"
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>

struct ap_grouped_site {const char *symbol;unsigned offset;};
static const struct ap_grouped_site sites[]={
#define AP_GROUP_ROW(role,symbol,address,offset,cookie) {#symbol,offset},
    AP_GROUPED_SITES(AP_GROUP_ROW)
#undef AP_GROUP_ROW
};
_Static_assert(sizeof(sites)/sizeof(sites[0])==AP_GROUPED_SITE_COUNT,"all seventeen physical sites");
static int invalid(void) {errno=EINVAL;return -1;}
static int bad(void) {errno=ENODATA;return -1;}
static int unknown(struct ap_grouped_owner *s) {s->phase=AP_GROUPED_UNKNOWN;return bad();}
static int name_byte(unsigned char c) {
    return (c>='a'&&c<='z')||(c>='A'&&c<='Z')||(c>='0'&&c<='9')||c=='_';
}
static int valid(const struct ap_grouped_owner *s) {
    return s && s->incarnation && s->group[0] && s->event[0] &&
        memchr(s->group,0,sizeof(s->group)) && memchr(s->event,0,sizeof(s->event));
}
int ap_grouped_owner_init(struct ap_grouped_owner *s,uint64_t incarnation,const char *nonce,size_t n) {
    if(!s || !incarnation || !nonce || n!=AP_GROUPED_NONCE_BYTES)return invalid();
    unsigned nonzero=0;
    for(size_t i=0;i<n;i++) {
        unsigned char c=(unsigned char)nonce[i];
        if(!((c>='0'&&c<='9')||(c>='a'&&c<='f')))return invalid();
        nonzero|=c!='0';
    }
    if(!nonzero)return invalid();
    struct ap_grouped_owner fresh={.incarnation=incarnation};
    memcpy(fresh.group,"hermit_",7);memcpy(fresh.group+7,nonce,n);
    memcpy(fresh.event,"hermit_classic_",15);memcpy(fresh.event+15,nonce,n);
    *s=fresh;return 0;
}
int ap_grouped_owner_ack(struct ap_grouped_owner *s,uint64_t incarnation,const char *nonce,size_t n) {
    struct ap_grouped_owner expected;
    if(!valid(s) || s->phase!=AP_GROUPED_EMPTY ||
       ap_grouped_owner_init(&expected,incarnation,nonce,n) ||
       incarnation!=s->incarnation || strcmp(expected.group,s->group) || strcmp(expected.event,s->event))
        return invalid();
    s->phase=AP_GROUPED_ACKED;return 0;
}
int ap_grouped_command(const struct ap_grouped_owner *s,unsigned role,int remove,char *out,size_t cap) {
    if(!valid(s) || !role || role>AP_GROUPED_SITE_COUNT || (remove!=0&&remove!=1) || !out)return invalid();
    const struct ap_grouped_site *site=&sites[role-1];
    int n=snprintf(out,cap,"%c:%s/%s %s+%u\n",remove?'-':'p',s->group,s->event,site->symbol,site->offset);
    if(n<0 || (size_t)n>=cap)return invalid();
    return n;
}
/* Full seq-file input is bounded, newline-terminated, ASCII and NUL-free.
 * Foreign locations/arguments are not parsed as ours, but every name token
 * is parsed so a same event name in another group cannot hide profile rows. */
int ap_grouped_census(const struct ap_grouped_owner *s,const void *input,size_t n,uint32_t *out) {
    if(!valid(s)||(!input&&n)||!out||n>AP_GROUPED_CENSUS_BYTES)return invalid();
    const unsigned char *p=input;uint32_t seen=0;
    for(size_t at=0;at<n;) {
        size_t end=at;while(end<n&&p[end]!='\n')end++;
        if(end==n || end==at)return bad();
        for(size_t j=at;j<end;j++)if(p[j]<32||p[j]>126)return bad();
        size_t colon=at;while(colon<end&&p[colon]!=':')colon++;
        if(colon==end || (p[at]!='p'&&p[at]!='r'))return bad();
        for(size_t j=at+1;j<colon;j++)if(p[at]!='r'||p[j]<'0'||p[j]>'9')return bad();
        size_t slash=colon+1;while(slash<end&&name_byte(p[slash]))slash++;
        if(slash==colon+1||slash==end||p[slash]!='/')return bad();
        size_t space=slash+1;while(space<end&&name_byte(p[space]))space++;
        if(space==slash+1||space==end||p[space]!=' ')return bad();
        const int group=slash-colon-1==strlen(s->group)&&!memcmp(p+colon+1,s->group,slash-colon-1);
        const int event=space-slash-1==strlen(s->event)&&!memcmp(p+slash+1,s->event,space-slash-1);
        if(group||event) {
            if(!group||!event)return bad();
            unsigned role=0;char exact[AP_GROUPED_LINE_BYTES];
            for(unsigned i=1;i<=AP_GROUPED_SITE_COUNT;i++) {
                int length=ap_grouped_command(s,i,0,exact,sizeof(exact));
                if(length<0)return -1;
                if((size_t)length==end-at+1&&!memcmp(p+at,exact,(size_t)length)) {role=i;break;}
            }
            if(!role||(seen&(1U<<(role-1))))return bad();
            seen|=1U<<(role-1);
        }
        at=end+1;
    }
    *out=seen;return 0;
}
static int number(const unsigned char *p,size_t n,size_t *at,uint64_t *value) {
    size_t start=*at;uint64_t x=0;
    while(*at<n&&p[*at]>='0'&&p[*at]<='9') {
        unsigned d=p[(*at)++]-'0';if(x>(UINT64_MAX-d)/10)return bad();x=x*10+d;
    }
    if(*at==start)return bad();
    *value=x;return 0;
}
int ap_grouped_profile(const struct ap_grouped_owner *s,const void *input,size_t n,unsigned expected) {
    if(!valid(s)||(!input&&n)||n>AP_GROUPED_CENSUS_BYTES||expected>AP_GROUPED_SITE_COUNT)return invalid();
    const unsigned char *p=input;unsigned count=0;
    for(size_t at=0;at<n;) {
        size_t row=at;
        if(n-at<2||p[at++]!=' '||p[at++]!=' ')return bad();
        size_t start=at;while(at<n&&name_byte(p[at]))at++;
        if(start==at)return bad();
        size_t name_n=at-start;
        if(name_n>255)return bad();
        int owned=name_n==strlen(s->event)&&!memcmp(p+start,s->event,name_n);
        if(at==n||p[at]!=' ')return bad();
        while(at<n&&p[at]==' ')at++;
        uint64_t hits,misses;if(number(p,n,&at,&hits))return -1;
        if(at==n||p[at]!=' ')return bad();
        while(at<n&&p[at]==' ')at++;
        if(number(p,n,&at,&misses)||at==n||p[at++]!='\n')return bad();
        char exact[512];int length=snprintf(exact,sizeof(exact),"  %-44.*s %15llu %15llu\n",
            (int)name_n,p+start,(unsigned long long)hits,(unsigned long long)misses);
        if(length<0||(size_t)length>=sizeof(exact)||(size_t)length!=at-row||memcmp(exact,p+row,at-row))return bad();
        if(owned&&(misses||++count>expected))return bad();
    }
    return count==expected?0:bad();
}
int ap_grouped_create_begin(struct ap_grouped_owner *s,const void *census,size_t n,unsigned role,char *line,size_t cap) {
    if(!valid(s)||(s->phase!=AP_GROUPED_ACKED&&s->phase!=AP_GROUPED_CREATING)||
       !role||role>AP_GROUPED_SITE_COUNT||s->write_unknown||s->pending_role)return invalid();
    uint32_t found;const uint32_t bit=1U<<(role-1);
    if(ap_grouped_census(s,census,n,&found)||found!=s->verified_sites||(s->attempted_sites&bit))
        return unknown(s);
    int bytes=ap_grouped_command(s,role,0,line,cap);if(bytes<0)return -1;
    s->phase=AP_GROUPED_CREATING;s->attempted_sites|=bit;
    s->pending_role=role;s->pending_remove=0;s->pending_bytes=(size_t)bytes;return bytes;
}
int ap_grouped_create_observed(struct ap_grouped_owner *s,unsigned role,ssize_t raw,size_t submitted,
        const void *census,size_t n) {
    if(!valid(s)||s->phase!=AP_GROUPED_CREATING||!role||role>AP_GROUPED_SITE_COUNT)return invalid();
    uint32_t found,bit=1U<<(role-1);char exact[AP_GROUPED_LINE_BYTES];
    int bytes=ap_grouped_command(s,role,0,exact,sizeof(exact));
    if(bytes<0 || submitted!=(size_t)bytes || submitted!=s->pending_bytes ||
       role!=s->pending_role || s->pending_remove ||
       !(s->attempted_sites&bit)||(s->verified_sites&bit))return unknown(s);
    if(raw!=(ssize_t)submitted)s->write_unknown=1;
    if(ap_grouped_census(s,census,n,&found)||found!=(s->verified_sites|bit))return unknown(s);
    s->verified_sites=found;s->pending_role=0;s->pending_bytes=0;
    if(s->write_unknown)return unknown(s);
    if(found==AP_GROUPED_ALL_SITES)s->phase=AP_GROUPED_CREATED;
    return 0;
}
/* No fetched arguments: exact x86 event field ABI. Dynamic ID/name are parsed
 * separately; every remaining byte is mandatory. Actual named-event runtime
 * still has to qualify this installed-image contract before activation. */
static const char format_tail[]=
    "format:\n"
    "\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n"
    "\tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;\n"
    "\tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n"
    "\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\n"
    "\tfield:unsigned long __probe_ip;\toffset:8;\tsize:8;\tsigned:0;\n\n"
    "print fmt: \"(%lx)\", REC->__probe_ip\n";
int ap_grouped_bind_leaves(struct ap_grouped_owner *s,const void *id,size_t id_n,
        const void *format,size_t format_n,const void *enable,size_t enable_n) {
    if(!valid(s)||s->phase!=AP_GROUPED_CREATED||!id||!format||!enable||id_n>16||format_n>4096)return invalid();
    size_t at=0;uint64_t value;
    if(number(id,id_n,&at,&value)||!value||value>UINT32_MAX||at+1!=id_n||((const char *)id)[at]!='\n'||
       enable_n!=2||memcmp(enable,"0\n",2))return unknown(s);
    char header[256];int head=snprintf(header,sizeof(header),"name: %s\nID: %u\n",s->event,(unsigned)value);
    if(head<0||(size_t)head>=sizeof(header)||format_n!=(size_t)head+sizeof(format_tail)-1||
       memcmp(format,header,(size_t)head)||memcmp((const char *)format+head,format_tail,sizeof(format_tail)-1))return unknown(s);
    s->event_id=(uint32_t)value;s->phase=AP_GROUPED_LEAVES;return 0;
}
int ap_grouped_activate(struct ap_grouped_owner *s,const void *census,size_t n,const void *profile,size_t pn) {
    if(!valid(s)||s->phase!=AP_GROUPED_LEAVES||!s->event_id||s->write_unknown)return invalid();
    uint32_t found;
    if(ap_grouped_census(s,census,n,&found)||found!=AP_GROUPED_ALL_SITES||
       ap_grouped_profile(s,profile,pn,AP_GROUPED_SITE_COUNT))return unknown(s);
    s->phase=AP_GROUPED_ACTIVE;return 0;
}
int ap_grouped_quiescent(struct ap_grouped_owner *s) {
    if(!valid(s)||s->phase!=AP_GROUPED_ACTIVE)return invalid();
    s->phase=AP_GROUPED_QUIESCENT;return 0;
}
int ap_grouped_recover(struct ap_grouped_owner *s,const void *census,size_t n) {
    if(!valid(s)||!s->attempted_sites||s->phase==AP_GROUPED_ACTIVE||s->phase==AP_GROUPED_ABSENT)return invalid();
    uint32_t found;
    if(ap_grouped_census(s,census,n,&found)||(found&~s->attempted_sites))return unknown(s);
    s->verified_sites=found;s->phase=AP_GROUPED_QUIESCENT;
    s->pending_role=0;s->pending_remove=0;s->pending_bytes=0;return 0;
}
int ap_grouped_delete_begin(struct ap_grouped_owner *s,const void *census,size_t n,unsigned role,char *line,size_t cap) {
    if(!valid(s)||(s->phase!=AP_GROUPED_QUIESCENT&&s->phase!=AP_GROUPED_CLEANING)||
       !role||role>AP_GROUPED_SITE_COUNT||s->pending_role)return invalid();
    uint32_t found,bit=1U<<(role-1);
    if(ap_grouped_census(s,census,n,&found)||found!=s->verified_sites||!(found&bit))return unknown(s);
    int bytes=ap_grouped_command(s,role,1,line,cap);if(bytes<0)return -1;
    s->phase=AP_GROUPED_CLEANING;s->pending_role=role;s->pending_remove=1;
    s->pending_bytes=(size_t)bytes;return bytes;
}
int ap_grouped_delete_observed(struct ap_grouped_owner *s,unsigned role,ssize_t raw,size_t submitted,
        const void *census,size_t n) {
    if(!valid(s)||s->phase!=AP_GROUPED_CLEANING||!role||role>AP_GROUPED_SITE_COUNT)return invalid();
    uint32_t found,bit=1U<<(role-1);char exact[AP_GROUPED_LINE_BYTES];
    int bytes=ap_grouped_command(s,role,1,exact,sizeof(exact));
    if(bytes<0||submitted!=(size_t)bytes||submitted!=s->pending_bytes||
       role!=s->pending_role||s->pending_remove!=1||!(s->verified_sites&bit))return unknown(s);
    if(raw!=(ssize_t)submitted)s->write_unknown=1;
    if(ap_grouped_census(s,census,n,&found)||found!=(s->verified_sites&~bit))return unknown(s);
    s->verified_sites=found;s->pending_role=0;s->pending_remove=0;s->pending_bytes=0;
    if(raw!=(ssize_t)submitted)return unknown(s);
    return 0;
}
int ap_grouped_absent(struct ap_grouped_owner *s,const void *census,size_t n,
        const void *profile,size_t pn,int directory_absent) {
    if(!valid(s)||(s->phase!=AP_GROUPED_CLEANING&&s->phase!=AP_GROUPED_QUIESCENT)||
       s->verified_sites||s->pending_role||directory_absent!=1)return invalid();
    uint32_t found;
    if(ap_grouped_census(s,census,n,&found)||found||ap_grouped_profile(s,profile,pn,0))return unknown(s);
    s->phase=AP_GROUPED_ABSENT;return 0;
}
