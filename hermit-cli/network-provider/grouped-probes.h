/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause */
#ifndef HERMIT_GROUPED_PROBES_H
#define HERMIT_GROUPED_PROBES_H
/* Exact installed-image physical sites. A common link cookie is NOT an old
 * semantic cookie. Actual PC, using the authenticated Connect entry anchor,
 * chooses one physical role before any original operand/frame validator. */
#define AP_GROUPED_COOKIE 0x4845524d49544731ULL
#define AP_GROUPED_CONNECT_IMAGE 0xffffffff8206bf70ULL
#define AP_GROUPED_VMEMMAP_BASE_IMAGE 0xffffffff82c3f2b0ULL
#define AP_GROUPED_PAGE_OFFSET_BASE_IMAGE 0xffffffff82c3f2c0ULL
#define AP_GROUPED_SITE_COUNT 17U
#define AP_GROUPED_ALL_SITES ((1U<<AP_GROUPED_SITE_COUNT)-1U)
#define AP_GROUPED_SITES(X) \
 X(1,__sys_connect,0xffffffff8206bf70ULL,0x1c,9) \
 X(2,__sys_connect,0xffffffff8206bf70ULL,0x41,3) \
 X(3,__sys_connect,0xffffffff8206bf70ULL,0x46,5) \
 X(4,__sys_accept4,0xffffffff82197db0ULL,0x21,8) \
 X(5,fdget_raw,0xffffffff81faaca0ULL,0x7c,13) \
 X(6,fdget_raw,0xffffffff81faaca0ULL,0x5,12) \
 X(7,__x64_sys_read,0xffffffff81fae890ULL,0x13,14) \
 X(8,fdget_pos,0xffffffff81faede0ULL,0x96,15) \
 X(9,fdget_pos,0xffffffff81faede0ULL,0xfa,16) \
 X(10,do_epoll_ctl,0xffffffff81fb1ca0ULL,0x23,18) \
 X(11,do_epoll_ctl,0xffffffff81fb1ca0ULL,0x37,19) \
 X(12,__skb_datagram_iter,0xffffffff81fb8970ULL,0x64,5) \
 X(13,__skb_datagram_iter,0xffffffff81fb8970ULL,0x69,6) \
 X(14,__skb_datagram_iter,0xffffffff81fb8970ULL,0x26b,7) \
 X(15,__skb_datagram_iter,0xffffffff81fb8970ULL,0x270,8) \
 X(16,inet_recvmsg,0xffffffff82355dc0ULL,0x1b,32) \
 X(17,inet6_recvmsg,0xffffffff820524a0ULL,0x1b,33)
static __attribute__((always_inline)) inline unsigned long long
ap_grouped_image_address(unsigned long long anchor,unsigned long long image) {
    if(anchor<0xffff800000000000ULL ||
       (anchor&4095)!=(AP_GROUPED_CONNECT_IMAGE&4095))return 0;
    if(image<0xffff800000000000ULL)return 0;
    if(image>=AP_GROUPED_CONNECT_IMAGE) {
        const unsigned long long delta=image-AP_GROUPED_CONNECT_IMAGE;
        return anchor<=~0ULL-delta?anchor+delta:0;
    }
    const unsigned long long delta=AP_GROUPED_CONNECT_IMAGE-image;
    return anchor>delta && anchor-delta>=0xffff800000000000ULL?anchor-delta:0;
}
static __attribute__((always_inline)) inline unsigned long long
ap_grouped_site_ip(unsigned role,unsigned long long anchor) {
    unsigned long long image;
    switch(role) {
#define AP_GROUP_IMAGE(role,symbol,address,offset,cookie) case role:image=address+offset+1;break;
    AP_GROUPED_SITES(AP_GROUP_IMAGE)
#undef AP_GROUP_IMAGE
    default:return 0;
    }
    return ap_grouped_image_address(anchor,image);
}
static __attribute__((always_inline)) inline unsigned
ap_grouped_site_role(unsigned long long anchor,unsigned long long ip) {
    if(anchor<0xffff800000000000ULL || ip<0xffff800000000000ULL ||
       (anchor&4095)!=(AP_GROUPED_CONNECT_IMAGE&4095))return 0;
    /* Translate one actual PC back into the installed-image coordinate with
     * checked arithmetic, then choose exactly one fixed physical role. */
    unsigned long long image;
    if(ip>=anchor) {
        unsigned long long delta=ip-anchor;
        if(delta>~0ULL-AP_GROUPED_CONNECT_IMAGE)return 0;
        image=AP_GROUPED_CONNECT_IMAGE+delta;
    } else {
        unsigned long long delta=anchor-ip;
        if(delta>AP_GROUPED_CONNECT_IMAGE)return 0;
        image=AP_GROUPED_CONNECT_IMAGE-delta;
    }
    switch(image) {
#define AP_GROUP_ROLE(role,symbol,address,offset,cookie) case address+offset+1:return role;
    AP_GROUPED_SITES(AP_GROUP_ROLE)
#undef AP_GROUP_ROLE
    default:return 0;
    }
}
static __attribute__((always_inline)) inline unsigned long long
ap_grouped_semantic_cookie(unsigned role) {
    switch(role) {
#define AP_GROUP_COOKIE(role,symbol,address,offset,cookie) case role:return cookie;
    AP_GROUPED_SITES(AP_GROUP_COOKIE)
#undef AP_GROUP_COOKIE
    default:return 0;
    }
}
#endif
