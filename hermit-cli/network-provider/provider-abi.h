#ifndef HERMIT_ACCEPTED_PROVIDER_ABI_H
#define HERMIT_ACCEPTED_PROVIDER_ABI_H
/* btf_ctx_arg_idx rounds each nonpointer argument size to8 bytes. sockptr_t
 * is passed BY VALUE (16 bytes), so parameter count is not ctx word count.
 * The runner independently pins the exact BTF hash used for these types. */
#define AP_CTX_SLOTS(type) ((sizeof(type)+sizeof(u64)-1)/sizeof(u64))
enum {
    AP_GETSOCKOPT_RESULT_SLOT = AP_CTX_SLOTS(struct sock *) + AP_CTX_SLOTS(int) +
        AP_CTX_SLOTS(int) + AP_CTX_SLOTS(sockptr_t) + AP_CTX_SLOTS(sockptr_t),
    AP_SETSOCKOPT_RESULT_SLOT = AP_CTX_SLOTS(struct sock *) + AP_CTX_SLOTS(int) +
        AP_CTX_SLOTS(int) + AP_CTX_SLOTS(sockptr_t) + AP_CTX_SLOTS(unsigned int),
    AP_CLONE_RESULT_SLOT = 7,
    AP_QUEUE_RESULT_SLOT = 3, /* retained proof for former queue-add target */
    AP_HASHDANCE_RESULT_SLOT = AP_CTX_SLOTS(struct sock *) + AP_CTX_SLOTS(struct sock *) +
        AP_CTX_SLOTS(struct request_sock *) + AP_CTX_SLOTS(bool),
};
_Static_assert(sizeof(u64)==8 && sizeof(void *)==8 && sizeof(int)==4,"reviewed64-bit ABI");
_Static_assert(sizeof(sockptr_t)==16,"actual BTF sockptr_t aggregate width");
_Static_assert(AP_HASHDANCE_RESULT_SLOT==4,"hashdance3 pointers+bool return slot4");
_Static_assert(AP_GETSOCKOPT_RESULT_SLOT==7,"getter return follows seven ctx words");
_Static_assert(AP_SETSOCKOPT_RESULT_SLOT==6,"setter return follows six ctx words");
static __attribute__((always_inline)) inline s32 ap_getsockopt_result(u64 *ctx) {
    return (s32)ctx[AP_GETSOCKOPT_RESULT_SLOT];
}
static __attribute__((always_inline)) inline s32 ap_setsockopt_result(u64 *ctx) {
    return (s32)ctx[AP_SETSOCKOPT_RESULT_SLOT];
}
#endif
