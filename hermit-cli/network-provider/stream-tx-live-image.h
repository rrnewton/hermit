/* SPDX-License-Identifier: MIT */
#ifndef HERMIT_STREAM_TX_LIVE_IMAGE_H
#define HERMIT_STREAM_TX_LIVE_IMAGE_H
/* Exact patch-free windows from the same full image as stream-tx-image.h.
 * Scalar immediates keep BPF expectations out of an unauthenticated rodata map.
 * The caller zeroes the last partial word before the bounded kernel read. */
#define AP_TX_PREFIX_OFFSET 0x28ULL
#define AP_TX_PREFIX_SIZE 227U
static __attribute__((always_inline)) inline int ap_tx_prefix_words(const u64 w[29]) {
    return w[0]==0x000000004c2444c7ULL &&
        w[1]==0x882484c744778b41ULL &&
        w[2]==0x4800000000000000ULL &&
        w[3]==0x00000000802484c7ULL &&
        w[4]==0x902484c748000000ULL &&
        w[5]==0x4800000000000000ULL &&
        w[6]==0x00000000982484c7ULL &&
        w[7]==0x000170838b000000ULL &&
        w[8]==0x0000008c24848900ULL &&
        w[9]==0x74894800487f8349ULL &&
        w[10]==0x00001069850f1024ULL &&
        w[11]==0x940f04000000c6f7ULL &&
        w[12]==0x08c1940fe4854dc0ULL &&
        w[13]==0x840f08247c894cc1ULL &&
        w[14]==0x4747f6410000106cULL &&
        w[15]==0x480000107f850f08ULL &&
        w[16]==0x00000000402444c7ULL &&
        w[17]==0x000000282444c748ULL &&
        w[18]==0x0000182444c74800ULL &&
        w[19]==0x00009824bc830000ULL &&
        w[20]==0x0fed85c0950f0000ULL &&
        w[21]==0x0fc08400000bdc85ULL &&
        w[22]==0x00c6f70000109685ULL &&
        w[23]==0x000975850f200000ULL &&
        w[24]==0x0000033883f74800ULL &&
        w[25]==0x0964850f00080000ULL &&
        w[26]==0x840f40c6f6400000ULL &&
        w[27]==0x8948c03100000d5eULL &&
        w[28]==0x0000000000782444ULL;
}
#define AP_TX_LOAD_OFFSET 0xe62ULL
#define AP_TX_LOAD_SIZE 12U
static __attribute__((always_inline)) inline int ap_tx_load_words(const u64 w[2]) {
    return w[0]==0xe900000210838b48ULL &&
        w[1]==0x00000000fffff298ULL;
}
#define AP_TX_WAIT_MEMORY_OFFSET 0xb27ULL
#define AP_TX_WAIT_MEMORY_SIZE 13U
static __attribute__((always_inline)) inline int ap_tx_wait_memory_words(const u64 w[2]) {
    return w[0]==0xdf89487824748d48ULL &&
        w[1]==0x00000000174a2ce8ULL;
}
#define AP_TX_WAIT_CONNECT_OFFSET 0xcc4ULL
#define AP_TX_WAIT_CONNECT_SIZE 13U
static __attribute__((always_inline)) inline int ap_tx_wait_connect_words(const u64 w[2]) {
    return w[0]==0xdf89487824748d48ULL &&
        w[1]==0x0000000017300fe8ULL;
}
#endif
