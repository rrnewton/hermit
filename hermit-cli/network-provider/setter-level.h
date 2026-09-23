#ifndef AP_SETTER_LEVEL_H
#define AP_SETTER_LEVEL_H
#include "provider.h"
/* sk_setsockopt and sk_getsockopt implement SOL_SOCKET options; level is
 * unused in both bodies. The running setter caller does not restore that
 * register; the running getter caller happens to, without making an unused
 * parameter ABI authority. Raw ctx1 is non-authoritative. Submitted setter
 * expected_level is STILL compared to1; the getter still requires SO_COOKIE.
 * tcp_setsockopt uses its actual level and must not use this helper. */
static __attribute__((always_inline)) inline s32 ap_socket_semantic_level(s32 raw_unused_level) {
    (void)raw_unused_level;
    return AP_SOL_SOCKET;
}
#endif
