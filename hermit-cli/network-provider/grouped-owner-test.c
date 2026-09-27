/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#include "grouped-owner.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
static const char nonce[]="0123456789abcdef0123456789abcdef";
static unsigned checks;
#define CHECK(x) do {checks++;assert(x);} while(0)
static struct ap_grouped_owner fresh(void) {
    struct ap_grouped_owner s;
    CHECK(ap_grouped_owner_init(&s,123,nonce,32)==0);
    CHECK(ap_grouped_owner_ack(&s,123,nonce,32)==0);return s;
}
static size_t census(const struct ap_grouped_owner *s,unsigned mask,char *out) {
    size_t n=0;
    for(unsigned role=1;role<=17;role++)if(mask&(1U<<(role-1))) {
        int len=ap_grouped_command(s,role,0,out+n,8192-n);CHECK(len>0);n+=(size_t)len;
    }
    return n;
}
static size_t profile(const struct ap_grouped_owner *s,unsigned count,unsigned miss,char *out) {
    size_t n=0;
    for(unsigned i=0;i<count;i++) {
        int len=snprintf(out+n,8192-n,"  %-44s %15llu %15llu\n",s->event,(unsigned long long)i,(unsigned long long)miss);
        CHECK(len>0&&(size_t)len<8192-n);n+=(size_t)len;
    }
    return n;
}
static size_t format(const struct ap_grouped_owner *s,char *out) {
    int n=snprintf(out,4096,
        "name: %s\nID: 12345\nformat:\n"
        "\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n"
        "\tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;\n"
        "\tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n"
        "\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\n"
        "\tfield:unsigned long __probe_ip;\toffset:8;\tsize:8;\tsigned:0;\n\n"
        "print fmt: \"(%%lx)\", REC->__probe_ip\n",s->event);
    CHECK(n>0&&n<4096);return (size_t)n;
}
static struct ap_grouped_owner created(void) {
    struct ap_grouped_owner s=fresh();char rows[8192],line[256];unsigned mask=0;
    for(unsigned role=1;role<=17;role++) {
        size_t n=census(&s,mask,rows);int len=ap_grouped_create_begin(&s,rows,n,role,line,sizeof(line));CHECK(len>0);
        struct ap_grouped_owner retained=s;
        CHECK(ap_grouped_create_begin(&s,rows,n,role,line,sizeof(line))==-1);
        CHECK(!memcmp(&s,&retained,sizeof(s)));
        mask|=1U<<(role-1);n=census(&s,mask,rows);
        CHECK(ap_grouped_create_observed(&s,role,len,(size_t)len,rows,n)==0);
        CHECK(s.verified_sites==mask&&s.pending_role==0);
    }
    CHECK(s.phase==AP_GROUPED_CREATED);return s;
}
static void role_controls(void) {
    for(unsigned slide=0;slide<2;slide++) {
        const unsigned long long anchor=AP_GROUPED_CONNECT_IMAGE+(unsigned long long)slide*0x400000;
        CHECK(ap_grouped_image_address(anchor,AP_GROUPED_CONNECT_IMAGE)==anchor);
        CHECK(ap_grouped_image_address(anchor,AP_GROUPED_VMEMMAP_BASE_IMAGE)==
            AP_GROUPED_VMEMMAP_BASE_IMAGE+(unsigned long long)slide*0x400000);
        CHECK(ap_grouped_image_address(anchor,AP_GROUPED_PAGE_OFFSET_BASE_IMAGE)==
            AP_GROUPED_PAGE_OFFSET_BASE_IMAGE+(unsigned long long)slide*0x400000);
        for(unsigned role=1;role<=17;role++) {
            unsigned long long ip=ap_grouped_site_ip(role,anchor);
            CHECK(ip&&ap_grouped_site_role(anchor,ip)==role);
            CHECK(!ap_grouped_site_role(anchor,ip-1));CHECK(!ap_grouped_site_role(anchor,ip+1));
            CHECK(ap_grouped_semantic_cookie(role));
            CHECK(!ap_grouped_site_role(anchor+1,ip));
            CHECK(!ap_grouped_site_ip(role,0));
            CHECK(!ap_grouped_site_ip(role,AP_GROUPED_CONNECT_IMAGE&0x7fffffffffffffffULL));
        }
    }
    CHECK(ap_grouped_semantic_cookie(3)==5&&ap_grouped_semantic_cookie(12)==5);
    CHECK(ap_grouped_semantic_cookie(4)==8&&ap_grouped_semantic_cookie(15)==8);
    CHECK(!ap_grouped_site_ip(0,AP_GROUPED_CONNECT_IMAGE));CHECK(!ap_grouped_site_ip(18,AP_GROUPED_CONNECT_IMAGE));
    CHECK(!ap_grouped_site_role(AP_GROUPED_CONNECT_IMAGE,0));CHECK(!ap_grouped_semantic_cookie(18));
    CHECK(!ap_grouped_site_ip(16,0xffffffffffffbf70ULL));
    CHECK(!ap_grouped_site_ip(5,0xffff80000000bf70ULL));
    CHECK(!ap_grouped_image_address(0,AP_GROUPED_VMEMMAP_BASE_IMAGE));
    CHECK(!ap_grouped_image_address(AP_GROUPED_CONNECT_IMAGE&0x7fffffffffffffffULL,
        AP_GROUPED_VMEMMAP_BASE_IMAGE));
    CHECK(!ap_grouped_image_address(0xffffffffffffbf70ULL,AP_GROUPED_VMEMMAP_BASE_IMAGE));
    CHECK(!ap_grouped_image_address(0xffff80000000bf70ULL,0xffff800000000000ULL));
}
/* Independent fixed literals, not expanded from the production X-macro. */
static void exact_site_controls(void) {
    static const struct {const char *tail;unsigned long long ip,cookie;} exact[]={
        {"__sys_connect+28\n",0xffffffff8206bf8dULL,9},
        {"__sys_connect+65\n",0xffffffff8206bfb2ULL,3},
        {"__sys_connect+70\n",0xffffffff8206bfb7ULL,5},
        {"__sys_accept4+33\n",0xffffffff82197dd2ULL,8},
        {"fdget_raw+124\n",0xffffffff81faad1dULL,13},
        {"fdget_raw+5\n",0xffffffff81faaca6ULL,12},
        {"__x64_sys_read+19\n",0xffffffff81fae8a4ULL,14},
        {"fdget_pos+150\n",0xffffffff81faee77ULL,15},
        {"fdget_pos+250\n",0xffffffff81faeedbULL,16},
        {"do_epoll_ctl+35\n",0xffffffff81fb1cc4ULL,18},
        {"do_epoll_ctl+55\n",0xffffffff81fb1cd8ULL,19},
        {"__skb_datagram_iter+100\n",0xffffffff81fb89d5ULL,5},
        {"__skb_datagram_iter+105\n",0xffffffff81fb89daULL,6},
        {"__skb_datagram_iter+619\n",0xffffffff81fb8bdcULL,7},
        {"__skb_datagram_iter+624\n",0xffffffff81fb8be1ULL,8},
        {"inet_recvmsg+27\n",0xffffffff82355ddcULL,32},
        {"inet6_recvmsg+27\n",0xffffffff820524bcULL,33},
    };
    struct ap_grouped_owner s=fresh();char line[256],expected[256],bad[256];
    CHECK(sizeof(exact)/sizeof(exact[0])==17);
    for(unsigned i=0;i<17;i++) {
        int n=snprintf(expected,sizeof(expected),"p:hermit_0123456789abcdef0123456789abcdef/hermit_classic_0123456789abcdef0123456789abcdef %s",exact[i].tail);
        CHECK(n>0 && n<(int)sizeof(expected));
        CHECK(ap_grouped_command(&s,i+1,0,line,sizeof(line))==n);
        CHECK(!memcmp(line,expected,(size_t)n));
        CHECK(ap_grouped_site_ip(i+1,0xffffffff8206bf70ULL)==exact[i].ip);
        CHECK(ap_grouped_site_role(0xffffffff8206bf70ULL,exact[i].ip)==i+1);
        CHECK(ap_grouped_semantic_cookie(i+1)==exact[i].cookie);
        size_t begin=(size_t)(strchr(expected,' ')-expected)+1;
        for(size_t j=begin;j<(size_t)n-1;j++) {
            memcpy(bad,expected,(size_t)n);bad[j]='Z';uint32_t mask=0xabcdef;
            CHECK(ap_grouped_census(&s,bad,(size_t)n,&mask)==-1);CHECK(mask==0xabcdef);
        }
    }
}

/* Installed trace_kprobe_show formats " %s+%u" and trace_kprobe_match
 * formats "%s+%u" before strcmp. These readback/deletion literals are
 * independent of ap_grouped_command and the production site X-macro.
 * Keep all historical hexadecimal locations as explicit refusals. */
static void kernel_decimal_controls(void) {
    static const struct {const char *decimal,*hex;} rows[]={
        {"__sys_connect+28\n","__sys_connect+0x1c\n"},
        {"__sys_connect+65\n","__sys_connect+0x41\n"},
        {"__sys_connect+70\n","__sys_connect+0x46\n"},
        {"__sys_accept4+33\n","__sys_accept4+0x21\n"},
        {"fdget_raw+124\n","fdget_raw+0x7c\n"},
        {"fdget_raw+5\n","fdget_raw+0x5\n"},
        {"__x64_sys_read+19\n","__x64_sys_read+0x13\n"},
        {"fdget_pos+150\n","fdget_pos+0x96\n"},
        {"fdget_pos+250\n","fdget_pos+0xfa\n"},
        {"do_epoll_ctl+35\n","do_epoll_ctl+0x23\n"},
        {"do_epoll_ctl+55\n","do_epoll_ctl+0x37\n"},
        {"__skb_datagram_iter+100\n","__skb_datagram_iter+0x64\n"},
        {"__skb_datagram_iter+105\n","__skb_datagram_iter+0x69\n"},
        {"__skb_datagram_iter+619\n","__skb_datagram_iter+0x26b\n"},
        {"__skb_datagram_iter+624\n","__skb_datagram_iter+0x270\n"},
        {"inet_recvmsg+27\n","inet_recvmsg+0x1b\n"},
        {"inet6_recvmsg+27\n","inet6_recvmsg+0x1b\n"},
    };
    const char prefix[]="p:hermit_0123456789abcdef0123456789abcdef/hermit_classic_0123456789abcdef0123456789abcdef ";
    CHECK(sizeof(rows)/sizeof(rows[0])==17);
    for(unsigned i=0;i<17;i++) {
        struct ap_grouped_owner s=fresh();char actual[256],readback[256],oldhex[256];
        int n=snprintf(readback,sizeof(readback),"%s%s",prefix,rows[i].decimal);
        int h=snprintf(oldhex,sizeof(oldhex),"%s%s",prefix,rows[i].hex);
        CHECK(n>0&&n<(int)sizeof(readback)&&h>0&&h<(int)sizeof(oldhex));
        uint32_t found=0xabcdef;
        CHECK(!ap_grouped_census(&s,readback,(size_t)n,&found)&&found==(1U<<i));
        found=0xabcdef;
        CHECK(ap_grouped_census(&s,oldhex,(size_t)h,&found)==-1&&found==0xabcdef);
        CHECK(ap_grouped_create_begin(&s,"",0,i+1,actual,sizeof(actual))==n);
        CHECK(!memcmp(actual,readback,(size_t)n));
        struct ap_grouped_owner bad=s;
        CHECK(ap_grouped_create_observed(&bad,i+1,n,(size_t)n,oldhex,(size_t)h)==-1);
        CHECK(bad.phase==AP_GROUPED_UNKNOWN&&bad.verified_sites==0);
        CHECK(!ap_grouped_create_observed(&s,i+1,n,(size_t)n,readback,(size_t)n));
        CHECK(s.verified_sites==(1U<<i));
        CHECK(!ap_grouped_recover(&s,readback,(size_t)n));
        CHECK(ap_grouped_delete_begin(&s,readback,(size_t)n,i+1,actual,sizeof(actual))==n);
        readback[0]='-';oldhex[0]='-';
        CHECK(!memcmp(actual,readback,(size_t)n));
        CHECK(strcmp(actual,oldhex)!=0);
        CHECK(!ap_grouped_delete_observed(&s,i+1,n,(size_t)n,"",0));
        CHECK(!ap_grouped_absent(&s,"",0,"",0,1));
        CHECK(s.phase==AP_GROUPED_ABSENT&&!s.verified_sites&&!s.pending_role);
    }
}

static void census_controls(void) {
    struct ap_grouped_owner s=fresh();char rows[8192],altered[8192];uint32_t out=42;
    size_t n=census(&s,AP_GROUPED_ALL_SITES,rows);
    CHECK(!ap_grouped_census(&s,rows,n,&out)&&out==AP_GROUPED_ALL_SITES);
    for(unsigned role=1;role<=17;role++) {
        size_t m=census(&s,AP_GROUPED_ALL_SITES&~(1U<<(role-1)),altered);out=0;
        CHECK(!ap_grouped_census(&s,altered,m,&out)&&out==(AP_GROUPED_ALL_SITES&~(1U<<(role-1))));
        int len=ap_grouped_command(&s,role,0,altered+m,sizeof(altered)-m);CHECK(len>0);
        int duplicate=ap_grouped_command(&s,role,0,altered+m+(size_t)len,sizeof(altered)-m-(size_t)len);CHECK(duplicate==len);
        out=42;CHECK(ap_grouped_census(&s,altered,m+(size_t)len+(size_t)duplicate,&out)==-1&&out==42);
    }
    for(size_t i=0;i<n;i++) {
        memcpy(altered,rows,n);altered[i]='\0';out=42;
        CHECK(ap_grouped_census(&s,altered,n,&out)==-1&&out==42);
    }
    for(size_t cut=1;cut<n;cut++)if(rows[cut-1]!='\n') {
        out=42;CHECK(ap_grouped_census(&s,rows,cut,&out)==-1&&out==42);
    }
    int len=snprintf(altered,sizeof(altered),"p:foreign/%s __sys_connect+0x1c\n",s.event);CHECK(len>0);
    CHECK(ap_grouped_census(&s,altered,(size_t)len,&out)==-1);
    len=snprintf(altered,sizeof(altered),"p:%s/foreign __sys_connect+0x1c\n",s.group);CHECK(len>0);
    CHECK(ap_grouped_census(&s,altered,(size_t)len,&out)==-1);
    const char foreign[]="r8:other/event other_fn+0x1 arg=%ax:u64\n";
    CHECK(!ap_grouped_census(&s,foreign,sizeof(foreign)-1,&out)&&out==0);
    CHECK(ap_grouped_census(&s,rows,AP_GROUPED_CENSUS_BYTES+1,&out)==-1);
}
static void owner_controls(void) {
    char rows[8192],line[256],prof[8192],fmt[4096];struct ap_grouped_owner s=created();
    size_t n=census(&s,AP_GROUPED_ALL_SITES,rows),pn=profile(&s,17,0,prof),fn=format(&s,fmt);
    struct ap_grouped_owner baseline=s;
    for(size_t i=0;i<fn;i++) {
        char old=fmt[i];fmt[i]^=1;s=baseline;
        CHECK(ap_grouped_bind_leaves(&s,"12345\n",6,fmt,fn,"0\n",2)==-1&&s.phase==AP_GROUPED_UNKNOWN);fmt[i]=old;
    }
    s=baseline;CHECK(ap_grouped_bind_leaves(&s,"12346\n",6,fmt,fn,"0\n",2)==-1);
    s=baseline;CHECK(ap_grouped_bind_leaves(&s,"12345\n",6,fmt,fn,"1\n",2)==-1);
    s=baseline;CHECK(!ap_grouped_bind_leaves(&s,"12345\n",6,fmt,fn,"0\n",2));
    CHECK(!ap_grouped_activate(&s,rows,n,prof,pn));CHECK(s.phase==AP_GROUPED_ACTIVE);
    CHECK(ap_grouped_delete_begin(&s,rows,n,1,line,sizeof(line))==-1);
    CHECK(ap_grouped_recover(&s,rows,n)==-1);
    CHECK(!ap_grouped_quiescent(&s));unsigned mask=AP_GROUPED_ALL_SITES;
    for(unsigned role=17;role;role--) {
        int len=ap_grouped_delete_begin(&s,rows,n,role,line,sizeof(line));CHECK(len>0&&line[0]=='-');
        struct ap_grouped_owner held=s;CHECK(ap_grouped_delete_begin(&s,rows,n,role,line,sizeof(line))==-1);
        CHECK(!memcmp(&s,&held,sizeof(s)));
        mask&=~(1U<<(role-1));n=census(&s,mask,rows);
        CHECK(!ap_grouped_delete_observed(&s,role,len,(size_t)len,rows,n));
    }
    CHECK(ap_grouped_absent(&s,rows,n,"",0,0)==-1);
    CHECK(!ap_grouped_absent(&s,rows,n,"",0,1)&&s.phase==AP_GROUPED_ABSENT);
    CHECK(ap_grouped_recover(&s,"",0)==-1);
    for(unsigned missing=0;missing<17;missing++) {
        pn=profile(&baseline,missing,0,prof);CHECK(ap_grouped_profile(&baseline,prof,pn,17)==-1);
    }
    pn=profile(&baseline,18,0,prof);CHECK(ap_grouped_profile(&baseline,prof,pn,17)==-1);
    pn=profile(&baseline,17,1,prof);CHECK(ap_grouped_profile(&baseline,prof,pn,17)==-1);
    pn=profile(&baseline,17,0,prof);CHECK(!ap_grouped_profile(&baseline,prof,pn,17));
    CHECK(ap_grouped_profile(&baseline,prof,pn-1,17)==-1);
    for(unsigned role=1;role<=17;role++) {
        s=fresh();n=0;int len=ap_grouped_create_begin(&s,rows,n,role,line,sizeof(line));CHECK(len>0);
        n=census(&s,1U<<(role-1),rows);
        CHECK(ap_grouped_create_observed(&s,role,len-1,(size_t)len,rows,n)==-1);
        CHECK(s.phase==AP_GROUPED_UNKNOWN&&s.write_unknown&&s.verified_sites==(1U<<(role-1)));
        CHECK(!ap_grouped_recover(&s,rows,n));
        len=ap_grouped_delete_begin(&s,rows,n,role,line,sizeof(line));CHECK(len>0);
        CHECK(!ap_grouped_delete_observed(&s,role,len,(size_t)len,"",0));
        CHECK(!ap_grouped_absent(&s,"",0,"",0,1)&&s.write_unknown);
    }
    s=fresh();n=census(&s,1,rows);
    CHECK(ap_grouped_create_begin(&s,rows,n,1,line,sizeof(line))==-1);
    CHECK(s.phase==AP_GROUPED_UNKNOWN&&s.attempted_sites==0);
    CHECK(ap_grouped_recover(&s,rows,n)==-1); /* never delete a pre-existing namespace */
    CHECK(ap_grouped_owner_init(&s,0,nonce,32)==-1);
    CHECK(ap_grouped_owner_init(&s,123,"00000000000000000000000000000000",32)==-1);
    CHECK(ap_grouped_owner_init(&s,123,"../23456789abcdef0123456789abcdef",32)==-1);
    CHECK(!ap_grouped_owner_init(&s,123,nonce,32));
    CHECK(ap_grouped_create_begin(&s,"",0,1,line,sizeof(line))==-1);
    CHECK(ap_grouped_owner_ack(&s,124,nonce,32)==-1);
    CHECK(!ap_grouped_owner_ack(&s,123,nonce,32));
    CHECK(ap_grouped_owner_ack(&s,123,nonce,32)==-1);
}
int main(void) {
    role_controls();exact_site_controls();census_controls();owner_controls();kernel_decimal_controls();
    printf("GROUPED_OWNER controls=%u sites=%u no_tracefs_effects=1\n",checks,AP_GROUPED_SITE_COUNT);return 0;
}
